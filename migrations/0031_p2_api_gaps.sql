-- SPDX-License-Identifier: GPL-2.0-or-later
-- P2 backend: API gaps reported by the P1 frontend (docs/impl/p1-backend-gaps.md).
--
--   * G13: proposal_contents.baseline — the target issues' values, at
--     proposal time, of the fields the proposal changes, as the proposing
--     agent could read them. It is content like the body: it lives in the
--     redactable row and is cleared with it (redaction::derived), never in
--     the append-only changeset_versions.
--   * G14: agent_credentials.last_used_at — written by bearer
--     authentication at most once a minute per token.
--
-- No role, permission or membership changes and no principal is inserted,
-- deleted or re-statused: security_epoch does not move.

ALTER TABLE proposal_contents
    ADD COLUMN baseline jsonb,
    ADD CONSTRAINT proposal_contents_baseline_redacted CHECK (state = 'available' OR baseline IS NULL);

-- 0009's guard plus the baseline: immutable, and cleared (to NULL) only by
-- redaction, which also leaves state 'available'.
CREATE OR REPLACE FUNCTION proposal_contents_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        RAISE EXCEPTION 'proposal_contents rows are not deleted';
    END IF;
    IF OLD.state <> 'available' AND NEW.state = 'available' THEN
        RAISE EXCEPTION 'redacted proposal content does not come back';
    END IF;
    IF NEW.body_sha256 <> OLD.body_sha256 OR NEW.id <> OLD.id
       OR (NEW.body IS NOT NULL AND NEW.body IS DISTINCT FROM OLD.body)
       OR (NEW.baseline IS NOT NULL AND NEW.baseline IS DISTINCT FROM OLD.baseline) THEN
        RAISE EXCEPTION 'proposal content is immutable except for redaction';
    END IF;
    -- The baseline is cleared only together with the body (redaction).
    IF NEW.baseline IS NULL AND OLD.baseline IS NOT NULL AND NEW.state = 'available' THEN
        RAISE EXCEPTION 'the baseline is cleared only by redaction';
    END IF;
    RETURN NEW;
END $$;

ALTER TABLE agent_credentials ADD COLUMN last_used_at timestamptz;
