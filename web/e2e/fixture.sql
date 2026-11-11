-- SPDX-License-Identifier: GPL-2.0-or-later
-- Applied after `oxledger seed-demo` on the frontend's own databases
-- (oxledger_fe_*), never on shared ones. seed-demo is read-oriented (S0);
-- the SPA flows need people who can write, and an AUTH project an agent may
-- be delegated on (contract 02 §3.8: no `own` role).

-- Writing permissions for members (Manager also manages milestones).
INSERT INTO role_permissions (role_id, permission)
SELECT r.id, p.perm FROM roles r
CROSS JOIN (VALUES ('add_issues'), ('edit_issues'), ('add_issue_notes'), ('set_issues_private'),
                   ('manage_issue_relations'), ('view_private_notes'), ('set_notes_private')) AS p(perm)
WHERE r.name IN ('Manager', 'Developer')
ON CONFLICT DO NOTHING;
INSERT INTO role_permissions (role_id, permission)
SELECT id, 'manage_versions' FROM roles WHERE name = 'Manager'
ON CONFLICT DO NOTHING;

-- AUTH becomes audience-consistent: Reporter sees the default set instead of `own`.
UPDATE roles SET issues_visibility = 'default' WHERE name = 'Reporter';

-- alice also works on WEB (cross-project usage 9), as Developer.
INSERT INTO members (project_id, principal_id)
SELECT p.id, u.id FROM projects p, principals u WHERE p.key = 'WEB' AND u.login = 'alice'
ON CONFLICT DO NOTHING;
INSERT INTO member_roles (member_id, role_id)
SELECT m.id, r.id FROM members m JOIN projects p ON p.id = m.project_id JOIN principals u ON u.id = m.principal_id, roles r
WHERE p.key = 'WEB' AND u.login = 'alice' AND r.name = 'Developer'
ON CONFLICT DO NOTHING;
