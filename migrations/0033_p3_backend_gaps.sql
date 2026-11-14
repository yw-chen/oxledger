-- SPDX-License-Identifier: GPL-2.0-or-later
-- P3 backend gaps reported by the frontend (docs/impl/p1-backend-gaps.md).
--
--   * G40: command_execution_chains — the agent chain of each issue a
--     direct agent write extended, as it stood after the write.
--     command_executions.agent_chain_writes keeps the largest of them (the
--     audit value of 0029); the issue history's `agent_chain` flag needs the
--     chain of the issue the event is on, or a flag on one issue would tell
--     that the same write touched another one (appendix D V21).
--
-- Append-only (like command_executions). Rows written before this
-- migration do not exist: those events show no flag.
--
-- No role, permission or membership changes and no principal is inserted,
-- deleted or re-statused: security_epoch does not move.

CREATE TABLE command_execution_chains (
    execution_id uuid NOT NULL REFERENCES execution_refs(id),
    issue_id     uuid NOT NULL REFERENCES issues(id),
    writes       integer NOT NULL CHECK (writes >= 1),
    PRIMARY KEY (execution_id, issue_id)
);

CREATE FUNCTION command_execution_chains_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'command_execution_chains rows are append-only';
END $$;
CREATE TRIGGER command_execution_chains_guard BEFORE UPDATE OR DELETE ON command_execution_chains
    FOR EACH ROW EXECUTE FUNCTION command_execution_chains_guard();
