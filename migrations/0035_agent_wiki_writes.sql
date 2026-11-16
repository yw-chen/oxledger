-- SPDX-License-Identifier: GPL-2.0-or-later
-- M2.3b (design 06 §3.12 R-AG6): agents write the wiki through
-- command_submit. A wiki write is counted per page in agent_write_log
-- (kind 'wiki_write', added to the CHECK by 0034): more than 10 writes to one
-- page in 10 minutes suspend the agent's context, as 10 writes to one issue
-- do (§16.6), with their own reason 'wiki_write_burst'.
--
-- No role, permission, membership or project change: security_epoch does
-- not move. Existing rows keep wiki_page_id NULL.

ALTER TABLE agent_write_log ADD COLUMN wiki_page_id uuid REFERENCES wiki_pages(id);
CREATE INDEX agent_write_log_wiki_page_idx ON agent_write_log (wiki_page_id, at)
    WHERE wiki_page_id IS NOT NULL;

-- The breaker's reason for a page written too often (the issue and version
-- bursts have their own; union rule, values verbatim from 0012).
ALTER TABLE agent_context_write_controls DROP CONSTRAINT agent_context_write_controls_suspension_reason_check;
ALTER TABLE agent_context_write_controls ADD CONSTRAINT agent_context_write_controls_suspension_reason_check
    CHECK (suspension_reason IN ('audience_blocked', 'issue_write_burst', 'version_write_burst', 'manual',
                                 'wiki_write_burst'));

-- Design 06 §4.4: the role template 「文件維護」 (`docs`): wiki_put and
-- wiki_move, reviewed (union rule, values verbatim from 0029).
ALTER TABLE agent_principals DROP CONSTRAINT agent_principals_role_template_check;
ALTER TABLE agent_principals ADD CONSTRAINT agent_principals_role_template_check
    CHECK (role_template IN ('personal','triage','pm_assistant','design_review',
                             'qa','release','builtin_chat','custom','docs'));
