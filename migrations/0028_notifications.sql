-- SPDX-License-Identifier: GPL-2.0-or-later
-- Design 05 M1.8 (§2.10, §3.11, A2, K9): watchers and the in-app
-- notification centre. No email (M2).
--
-- No role, permission or membership changes: security_epoch does not move.
-- Notifications hold references only (issue, event ids, reasons), never
-- text: what a row says is rebuilt from issue_events on every read with the
-- reader's permissions at that moment (§3.11.3), so a notification is not a
-- D12 copy and revoked access hides it on the next read.
-- Watchers and preferences are personal state: they are not issue events,
-- do not move lock_version and do not move any epoch.

-- ---------------------------------------------------------------- user_column_guard (appendix D V5)

-- 0005's user_only_guard() reads NEW.user_id; tables whose column has
-- another name (recipient_id, sponsor_user_id) need the column named in
-- TG_ARGV[0]. 0028–0030 use this one.
CREATE FUNCTION user_column_guard() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE v uuid := (to_jsonb(NEW) ->> TG_ARGV[0])::uuid;
BEGIN
    IF v IS NOT NULL AND NOT principal_kind_is(v, ARRAY['user']) THEN
        RAISE EXCEPTION '%.% must be a user', TG_TABLE_NAME, TG_ARGV[0]
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;

-- ---------------------------------------------------------------- watchers

-- Only users watch (02 §16.2: an agent is never a watcher); in M1 a person
-- watches or unwatches only themselves (B-24).
CREATE TABLE issue_watchers (
    issue_id   uuid NOT NULL REFERENCES issues(id),
    user_id    uuid NOT NULL REFERENCES principals(id),
    source     text NOT NULL CHECK (source IN ('manual','auto_author','auto_assignee','auto_note')),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (issue_id, user_id)
);
CREATE INDEX issue_watchers_user_idx ON issue_watchers (user_id);
CREATE TRIGGER issue_watchers_user_guard BEFORE INSERT OR UPDATE OF user_id ON issue_watchers
    FOR EACH ROW EXECUTE FUNCTION user_column_guard('user_id');

-- An explicit unwatch beats later automatic watching (being assigned again).
CREATE TABLE issue_watch_optouts (
    issue_id   uuid NOT NULL REFERENCES issues(id),
    user_id    uuid NOT NULL REFERENCES principals(id),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (issue_id, user_id)
);
CREATE TRIGGER issue_watch_optouts_user_guard BEFORE INSERT OR UPDATE OF user_id ON issue_watch_optouts
    FOR EACH ROW EXECUTE FUNCTION user_column_guard('user_id');

-- ---------------------------------------------------------------- notifications

-- One row = one recipient × one issue × a run of unread events. New events
-- of the issue join the unread row; once read, the next event opens a new
-- row. first_at / last_at are arrival (dispatch) times, so
-- `read-all {before}` never marks something that arrived after the list
-- was loaded.
CREATE TABLE notifications (
    id            uuid PRIMARY KEY DEFAULT uuidv7(),
    recipient_id  uuid NOT NULL REFERENCES principals(id),
    kind          text NOT NULL DEFAULT 'issue' CHECK (kind IN ('issue', 'ai_budget')),
    issue_id      uuid REFERENCES issues(id),
    project_id    uuid REFERENCES projects(id),
    -- issue_events.id in event order; beyond 50 the oldest are dropped.
    event_ids     uuid[] NOT NULL DEFAULT '{}' CHECK (cardinality(event_ids) <= 50),
    reasons       text[] NOT NULL CHECK (reasons <@ ARRAY['watching','assigned','author','agent_on_behalf',
                                                          'triage_decided','ai_budget']::text[]
                                         AND cardinality(reasons) >= 1),
    -- Parallel to event_ids: the reasons of each event, comma-joined and
    -- sorted (e.g. 'assigned,watching'); a reader is shown the union over
    -- the events they can still read, not `reasons` (the union over all).
    event_reasons text[] NOT NULL DEFAULT '{}' CHECK (cardinality(event_reasons) = cardinality(event_ids)),
    -- ai_budget only (M1.10): {"month", "threshold"}; amounts are computed on read.
    payload       jsonb CHECK (payload IS NULL OR kind = 'ai_budget'),
    first_at      timestamptz NOT NULL,
    last_at       timestamptz NOT NULL,
    read_at       timestamptz,
    CHECK (last_at >= first_at),
    CHECK ((kind = 'issue') = (issue_id IS NOT NULL AND project_id IS NOT NULL AND cardinality(event_ids) >= 1))
);
CREATE UNIQUE INDEX notifications_one_unread ON notifications (recipient_id, issue_id)
    WHERE read_at IS NULL AND kind = 'issue';
CREATE INDEX notifications_inbox_idx ON notifications (recipient_id, last_at DESC, id);
CREATE INDEX notifications_unread_idx ON notifications (recipient_id, last_at DESC, id) WHERE read_at IS NULL;
CREATE INDEX notifications_gc_idx ON notifications (last_at);
CREATE INDEX notifications_read_gc_idx ON notifications (read_at) WHERE read_at IS NOT NULL;
CREATE TRIGGER notifications_recipient_guard BEFORE INSERT OR UPDATE OF recipient_id ON notifications
    FOR EACH ROW EXECUTE FUNCTION user_column_guard('recipient_id');

-- ---------------------------------------------------------------- preferences

-- Created on the first GET /me/notification-preferences (or never: missing
-- = these defaults).
CREATE TABLE notification_preferences (
    user_id              uuid PRIMARY KEY REFERENCES principals(id),
    on_watching          boolean NOT NULL DEFAULT true,
    on_assigned          boolean NOT NULL DEFAULT true,
    on_author            boolean NOT NULL DEFAULT true,
    on_triage_decided    boolean NOT NULL DEFAULT true,
    on_my_agents         boolean NOT NULL DEFAULT true,
    on_self              boolean NOT NULL DEFAULT false,
    auto_watch_author    boolean NOT NULL DEFAULT true,
    auto_watch_assignee  boolean NOT NULL DEFAULT true,
    auto_watch_note      boolean NOT NULL DEFAULT true,
    lock_version         integer NOT NULL DEFAULT 0,
    updated_at           timestamptz NOT NULL DEFAULT now()
);
CREATE TRIGGER notification_preferences_user_guard BEFORE INSERT OR UPDATE OF user_id ON notification_preferences
    FOR EACH ROW EXECUTE FUNCTION user_column_guard('user_id');

CREATE TABLE notification_project_mutes (
    user_id    uuid NOT NULL REFERENCES principals(id),
    project_id uuid NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, project_id)
);
CREATE TRIGGER notification_project_mutes_user_guard BEFORE INSERT OR UPDATE OF user_id ON notification_project_mutes
    FOR EACH ROW EXECUTE FUNCTION user_column_guard('user_id');

-- ---------------------------------------------------------------- dispatch progress

-- The dispatch consumer's cursor. Since 0018 the outbox GC keeps every row
-- at or after the smallest cursor of all consumers in state active,
-- bootstrapping or retained, so an active `notify` holds the GC back while
-- it lags (search.rs `gc`; /metrics has notify_outbox_pending).
-- Dispatch starts now: events before the upgrade are not sent.
INSERT INTO outbox_cursors (consumer, last_xid, last_id, state)
    SELECT 'notify', pg_snapshot_xmin(pg_current_snapshot()), 0, 'active'
    ON CONFLICT (consumer) DO NOTHING;

-- Outbox messages carry only the issue id (at least once); this row says
-- up to which event an issue was dispatched, so a repeated message sends
-- nothing twice.
CREATE TABLE notification_issue_progress (
    issue_id       uuid PRIMARY KEY REFERENCES issues(id),
    last_event_seq bigint NOT NULL CHECK (last_event_seq >= 0)   -- issue_events.seq
);
-- Backfill (appendix D V7), in the transaction that registers the cursor:
-- an existing issue's progress is its last event now, so its first event
-- after the upgrade sends that event only. No row = an issue created after
-- the upgrade (or never dispatched yet): dispatch starts at seq 0.
INSERT INTO notification_issue_progress (issue_id, last_event_seq)
    SELECT issue_id, max(seq) FROM issue_events GROUP BY issue_id;
