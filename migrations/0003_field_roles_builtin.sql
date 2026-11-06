-- SPDX-License-Identifier: GPL-2.0-or-later
-- Custom field visibility may now list builtin roles (Non member, Anonymous).
--
-- 0002 refused them so that Redmine's two checks agreed: Redmine filters by member
-- roles only, but displays by roles_for_project (which adds the builtin role). oxledger
-- now uses one rule for both: a field with visible = false is readable in a project when
-- the caller's effective roles there (db::role_ids_for: member roles, or Non member on a
-- public project the caller is not a member of, or Anonymous when not signed in) include
-- one of field_roles. Admins read every field.
DROP TRIGGER field_roles_givable ON field_roles;
DROP FUNCTION field_roles_givable();
