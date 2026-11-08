-- SPDX-License-Identifier: GPL-2.0-or-later
-- Contract 02 S6: agent quotas and the circuit breaker (§8.9, §16.6).
--
-- agent_context_write_controls: the per-context control row. A direct write
-- takes it FOR UPDATE NOWAIT, so one write runs per context at a time (a
-- second one is told rate_limited instead of waiting); suspended_at is the
-- breaker. The row is outside the context row, so requests that hold the
-- context FOR SHARE never upgrade it (§16.6).
--
-- agent_write_log: what the counters count. Append-only; a row is written
-- in the transaction of the counted action (a write, a proposal), except
-- audience_blocked, which is written in its own transaction after the
-- refused request rolled back (A8).

CREATE TABLE agent_context_write_controls (
    context_id           uuid PRIMARY KEY REFERENCES agent_work_contexts(id),
    suspended_at         timestamptz,
    suspension_reason    text CHECK (suspension_reason IN ('audience_blocked', 'issue_write_burst',
                                                           'version_write_burst', 'manual')),
    inflight_lease_until timestamptz,   -- unused: the NOWAIT row lock is the lease (A9)
    CHECK ((suspended_at IS NULL) = (suspension_reason IS NULL))
);

CREATE TABLE agent_write_log (
    id                 uuid PRIMARY KEY DEFAULT uuidv7(),
    agent_principal_id uuid NOT NULL REFERENCES principals(id),
    context_id         uuid NOT NULL REFERENCES agent_work_contexts(id),
    delegation_id      uuid REFERENCES delegations(id),
    kind               text NOT NULL CHECK (kind IN ('direct_write', 'issue_create', 'issue_write',
                                                     'propose', 'audience_blocked',
                                                     'version_create', 'version_write')),
    issue_id           uuid REFERENCES issues(id) ON DELETE CASCADE,
    version_id         uuid,
    at                 timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX agent_write_log_agent_idx ON agent_write_log (agent_principal_id, kind, at);
CREATE INDEX agent_write_log_issue_idx ON agent_write_log (issue_id, at) WHERE issue_id IS NOT NULL;
CREATE INDEX agent_write_log_delegation_idx ON agent_write_log (delegation_id, kind, at)
    WHERE delegation_id IS NOT NULL;

CREATE FUNCTION agent_write_log_append_only() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    -- Cascades from a deleted issue are allowed; edits are not.
    IF TG_OP = 'UPDATE' THEN
        RAISE EXCEPTION 'agent_write_log is append-only' USING ERRCODE = 'check_violation';
    END IF;
    RETURN OLD;
END $$;
CREATE TRIGGER agent_write_log_append_only BEFORE UPDATE ON agent_write_log
    FOR EACH ROW EXECUTE FUNCTION agent_write_log_append_only();

-- Every context has its control row from the start, so the NOWAIT lock
-- never races an insert.
CREATE FUNCTION agent_context_controls_row() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO agent_context_write_controls (context_id) VALUES (NEW.id);
    RETURN NULL;
END $$;
CREATE TRIGGER agent_context_controls_row AFTER INSERT ON agent_work_contexts
    FOR EACH ROW EXECUTE FUNCTION agent_context_controls_row();
INSERT INTO agent_context_write_controls (context_id) SELECT id FROM agent_work_contexts;
