-- SPDX-License-Identifier: GPL-2.0-or-later
-- Contract 02 S6: projects.security_epoch triggers (§3.8) and project_catalog (§3.7).
--
-- Every change that can widen or narrow who reads a project's issues adds one
-- to the affected projects' security_epoch. Agent work contexts record the
-- epochs of their source projects R; any later difference quarantines the
-- whole context (§3.8 rule 5, M7). The trigger is the safety net: admin code
-- may lock projects itself first (§3.7), the trigger then finds them locked.
--
-- Lock order (§3.7): principals → project_catalog → projects (id order).
--   * all-project bumps take project_catalog FOR UPDATE, then list and lock
--     every project; creating a project takes project_catalog FOR SHARE, so
--     a project created during a bump waits and starts from the new settings
--     (T-X24);
--   * group changes lock the group principal row first, then look up the
--     group's projects (T-X23);
--   * principal changes (create / delete user, status, admin flag) hold the
--     principal row already (the statement itself locks it).
-- Not bumped: subject, note bodies, assignment (§3.8). field_defs INSERT and
-- DELETE are not bumped either: a new field applies to nothing until it is
-- wired with field_trackers / field_projects (which bump), and deleting a
-- field removes content (decisions.tsv, S6).

CREATE TABLE project_catalog (id boolean PRIMARY KEY DEFAULT true CHECK (id));
INSERT INTO project_catalog VALUES (true);

-- Bump the given projects, locking them in id order.
CREATE FUNCTION bump_projects(ids uuid[]) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    IF ids IS NULL OR cardinality(ids) = 0 THEN
        RETURN;
    END IF;
    PERFORM 1 FROM projects WHERE id = ANY(ids) ORDER BY id FOR UPDATE;
    UPDATE projects SET security_epoch = security_epoch + 1 WHERE id = ANY(ids);
END $$;

-- Bump every project: the catalog first, then list (a new statement, so the
-- list sees projects committed while we waited) and lock in id order.
CREATE FUNCTION bump_all_projects() RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    PERFORM 1 FROM project_catalog FOR UPDATE;
    PERFORM 1 FROM projects ORDER BY id FOR UPDATE;
    UPDATE projects SET security_epoch = security_epoch + 1;
END $$;

CREATE FUNCTION epoch_all_projects() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM bump_all_projects();
    RETURN NULL;
END $$;

-- ---------------------------------------------------------------- projects

CREATE FUNCTION projects_catalog_share() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM 1 FROM project_catalog FOR SHARE;
    RETURN NEW;
END $$;
CREATE TRIGGER projects_catalog_share BEFORE INSERT ON projects
    FOR EACH ROW EXECUTE FUNCTION projects_catalog_share();

CREATE FUNCTION projects_epoch_self() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.is_public IS DISTINCT FROM NEW.is_public OR OLD.status IS DISTINCT FROM NEW.status THEN
        NEW.security_epoch := OLD.security_epoch + 1;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER projects_epoch_self BEFORE UPDATE OF is_public, status ON projects
    FOR EACH ROW EXECUTE FUNCTION projects_epoch_self();

CREATE FUNCTION enabled_modules_epoch() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        PERFORM bump_projects(ARRAY[OLD.project_id]);
    END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') THEN
        PERFORM bump_projects(ARRAY[NEW.project_id]);
    END IF;
    RETURN NULL;
END $$;
CREATE TRIGGER enabled_modules_epoch AFTER INSERT OR UPDATE OR DELETE ON enabled_modules
    FOR EACH ROW EXECUTE FUNCTION enabled_modules_epoch();

-- ---------------------------------------------------------------- membership

-- A group principal's row is locked before its projects are looked up.
CREATE FUNCTION lock_if_group(p uuid) RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    PERFORM 1 FROM principals WHERE id = p AND kind = 'group' FOR UPDATE;
END $$;

CREATE FUNCTION members_epoch() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    ps uuid[] := '{}';
BEGIN
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        PERFORM lock_if_group(OLD.principal_id);
        ps := ps || OLD.project_id;
    END IF;
    IF TG_OP IN ('INSERT', 'UPDATE') THEN
        PERFORM lock_if_group(NEW.principal_id);
        ps := ps || NEW.project_id;
    END IF;
    PERFORM bump_projects(ps);
    RETURN NULL;
END $$;
CREATE TRIGGER members_epoch AFTER INSERT OR UPDATE OR DELETE ON members
    FOR EACH ROW EXECUTE FUNCTION members_epoch();

CREATE FUNCTION member_roles_epoch() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    m uuid;
    p uuid;
    pr uuid;
BEGIN
    FOREACH m IN ARRAY CASE TG_OP
            WHEN 'INSERT' THEN ARRAY[NEW.member_id]
            WHEN 'DELETE' THEN ARRAY[OLD.member_id]
            ELSE ARRAY[OLD.member_id, NEW.member_id] END LOOP
        SELECT project_id, principal_id INTO p, pr FROM members WHERE id = m;
        IF FOUND THEN
            PERFORM lock_if_group(pr);
            PERFORM bump_projects(ARRAY[p]);
        END IF;
        -- A member deleted with its project or principal (cascade): the
        -- members trigger bumped already.
    END LOOP;
    RETURN NULL;
END $$;
CREATE TRIGGER member_roles_epoch AFTER INSERT OR UPDATE OR DELETE ON member_roles
    FOR EACH ROW EXECUTE FUNCTION member_roles_epoch();

CREATE FUNCTION group_members_epoch() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    g uuid;
BEGIN
    FOREACH g IN ARRAY CASE TG_OP
            WHEN 'INSERT' THEN ARRAY[NEW.group_id]
            WHEN 'DELETE' THEN ARRAY[OLD.group_id]
            ELSE ARRAY[OLD.group_id, NEW.group_id] END LOOP
        PERFORM 1 FROM principals WHERE id = g FOR UPDATE;
        -- A new statement after the lock: sees members committed meanwhile.
        PERFORM bump_projects(ARRAY(SELECT project_id FROM members WHERE principal_id = g));
    END LOOP;
    RETURN NULL;
END $$;
CREATE TRIGGER group_members_epoch AFTER INSERT OR UPDATE OR DELETE ON group_members
    FOR EACH ROW EXECUTE FUNCTION group_members_epoch();

-- ---------------------------------------------------------------- users (kind = 'user' only)

-- Agent, group and system principals are not readers in A(P); counting them
-- would quarantine every context whenever an agent is registered.
CREATE TRIGGER principals_epoch_insert AFTER INSERT ON principals
    FOR EACH ROW WHEN (NEW.kind = 'user') EXECUTE FUNCTION epoch_all_projects();
CREATE TRIGGER principals_epoch_delete AFTER DELETE ON principals
    FOR EACH ROW WHEN (OLD.kind = 'user') EXECUTE FUNCTION epoch_all_projects();
CREATE TRIGGER principals_epoch_update AFTER UPDATE OF status, is_admin ON principals
    FOR EACH ROW WHEN (NEW.kind = 'user' AND (OLD.status IS DISTINCT FROM NEW.status
                                              OR OLD.is_admin IS DISTINCT FROM NEW.is_admin))
    EXECUTE FUNCTION epoch_all_projects();

-- A principal's kind cannot change (guard from 0005).

-- ---------------------------------------------------------------- roles (all projects)

CREATE TRIGGER roles_epoch AFTER UPDATE OF issues_visibility ON roles
    FOR EACH ROW WHEN (OLD.issues_visibility IS DISTINCT FROM NEW.issues_visibility)
    EXECUTE FUNCTION epoch_all_projects();
CREATE TRIGGER role_permissions_epoch AFTER INSERT OR UPDATE OR DELETE ON role_permissions
    FOR EACH STATEMENT EXECUTE FUNCTION epoch_all_projects();
CREATE TRIGGER role_permission_trackers_epoch AFTER INSERT OR UPDATE OR DELETE ON role_permission_trackers
    FOR EACH STATEMENT EXECUTE FUNCTION epoch_all_projects();

-- ---------------------------------------------------------------- custom fields (all projects)

CREATE TRIGGER field_defs_epoch AFTER UPDATE OF visible, is_for_all ON field_defs
    FOR EACH ROW WHEN (OLD.visible IS DISTINCT FROM NEW.visible OR OLD.is_for_all IS DISTINCT FROM NEW.is_for_all)
    EXECUTE FUNCTION epoch_all_projects();
CREATE TRIGGER field_roles_epoch AFTER INSERT OR UPDATE OR DELETE ON field_roles
    FOR EACH STATEMENT EXECUTE FUNCTION epoch_all_projects();
CREATE TRIGGER field_trackers_epoch AFTER INSERT OR UPDATE OR DELETE ON field_trackers
    FOR EACH STATEMENT EXECUTE FUNCTION epoch_all_projects();
CREATE TRIGGER field_projects_epoch AFTER INSERT OR UPDATE OR DELETE ON field_projects
    FOR EACH STATEMENT EXECUTE FUNCTION epoch_all_projects();

-- ---------------------------------------------------------------- private flags (that project)

CREATE FUNCTION issues_private_epoch() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM bump_projects(ARRAY[NEW.project_id]);
    RETURN NULL;
END $$;
CREATE TRIGGER issues_private_epoch AFTER UPDATE OF is_private ON issues
    FOR EACH ROW WHEN (OLD.is_private IS DISTINCT FROM NEW.is_private)
    EXECUTE FUNCTION issues_private_epoch();

CREATE FUNCTION issue_notes_private_epoch() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM bump_projects(ARRAY(SELECT project_id FROM issues WHERE id = NEW.issue_id));
    RETURN NULL;
END $$;
CREATE TRIGGER issue_notes_private_epoch AFTER UPDATE OF is_private ON issue_notes
    FOR EACH ROW WHEN (OLD.is_private IS DISTINCT FROM NEW.is_private)
    EXECUTE FUNCTION issue_notes_private_epoch();
