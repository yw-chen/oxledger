-- SPDX-License-Identifier: GPL-2.0-or-later
-- Design 06 R-A7 (U18): a person's delete_attachment is undone within 30
-- days. Each execution that deletes or puts back an attachment keeps a row
-- here (as execution_wiki_changes does for pages): the undo of a delete
-- puts the file back, the undo of that undo deletes it again.
-- The design's later migrations (Git, fixed_by, import/export) move up by
-- one (§2.1: merge order = number order).

CREATE TABLE execution_attachment_changes (
    execution_id  uuid NOT NULL REFERENCES execution_refs(id),
    attachment_id uuid NOT NULL REFERENCES attachments(id),
    -- What the execution did: `deleted` (the file went), `restored` (it
    -- came back).
    change        text NOT NULL CHECK (change IN ('deleted', 'restored')),
    -- The undo that reverted this row (one undo per row).
    reverted_by   uuid REFERENCES execution_refs(id),
    PRIMARY KEY (execution_id, attachment_id)
);
CREATE INDEX execution_attachment_changes_attachment_idx
    ON execution_attachment_changes (attachment_id);
