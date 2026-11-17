-- SPDX-License-Identifier: GPL-2.0-or-later
-- Design 06 §3.13, R-A12 (D12): redacting an attachment. One record per
-- redacted attachment (never the original name or content); every row of
-- the same blob generation points at the record of the attachment that was
-- named. The design's later migrations (Git, fixed_by, import/export) move
-- up by one more (§2.1: merge order = number order).

CREATE TABLE attachment_redactions (
    id            uuid PRIMARY KEY DEFAULT uuidv7(),
    attachment_id uuid NOT NULL REFERENCES attachments(id),
    redacted_by   uuid NOT NULL REFERENCES principals(id),
    redacted_at   timestamptz NOT NULL DEFAULT now(),
    reason_code   text NOT NULL,
    reason        text NOT NULL            -- never the original text
);
CREATE INDEX attachment_redactions_attachment_idx ON attachment_redactions (attachment_id);
-- NOT VALID: rows written before this table existed are not checked (none
-- in a deployment: no route redacted attachments before it); new and
-- updated rows are.
ALTER TABLE attachments ADD FOREIGN KEY (redaction_id) REFERENCES attachment_redactions(id)
    NOT VALID;
