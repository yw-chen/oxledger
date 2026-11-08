-- SPDX-License-Identifier: GPL-2.0-or-later
-- Contract 02 S6b.3: the validate quota (§8.9: 120 per delegation per hour)
-- is counted in agent_write_log like the other agent quotas.

ALTER TABLE agent_write_log DROP CONSTRAINT agent_write_log_kind_check;
ALTER TABLE agent_write_log ADD CONSTRAINT agent_write_log_kind_check
    CHECK (kind IN ('direct_write', 'issue_create', 'issue_write', 'propose', 'audience_blocked',
                    'version_create', 'version_write', 'validate'));
