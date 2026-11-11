-- SPDX-License-Identifier: GPL-2.0-or-later
-- Design 05 M1.6 (§2.8, §3.8): personal preferences and the home page
-- (L4, U24). Interface roles only change defaults, never permissions.
--
-- No role, permission or membership changes: security_epoch does not move.
-- A row is created on the first GET /me/preferences or /me/home
-- (INSERT … ON CONFLICT DO NOTHING), not in the login transaction.
-- What the home widgets, shortcuts and default views name (views,
-- projects) is checked again on every read with the reader's visibility;
-- the protected widget list (U22) is checked by the application.

CREATE TABLE user_preferences (
    user_id           uuid PRIMARY KEY REFERENCES principals(id),   -- a user (trigger)
    interface_roles   text[] NOT NULL DEFAULT '{}'
                        CHECK (interface_roles <@ ARRAY['pm','designer','engineer']::text[]),
    onboarded_at      timestamptz,                -- NULL: ask for the interface roles at next sign-in
    theme             text NOT NULL DEFAULT 'dark' CHECK (theme IN ('dark','light','high_contrast','system')),
    density           text NOT NULL DEFAULT 'comfortable' CHECK (density IN ('comfortable','compact')),
    -- {"widgets": [{id, type, width, view_id?, limit?, project_id?}]} (§3.8)
    home              jsonb NOT NULL CHECK (jsonb_typeof(home) = 'object'),
    nav_shortcuts     jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(nav_shortcuts) = 'array'),
    -- {"<project_id>" | "*": "<view id>" | "builtin:list" | "builtin:board" | "builtin:timeline"}
    default_views     jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(default_views) = 'object'),
    lock_version      integer NOT NULL DEFAULT 0,  -- GET/PUT /me/preferences
    -- GET/PUT /me/home has its own optimistic lock, so editing the home in
    -- one tab does not conflict with changing the theme in another.
    home_lock_version integer NOT NULL DEFAULT 0,
    updated_at        timestamptz NOT NULL DEFAULT now()
);

CREATE FUNCTION user_preferences_user_check() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM principals WHERE id = NEW.user_id AND kind = 'user') THEN
        RAISE EXCEPTION 'user_preferences.user_id must be a user' USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER user_preferences_user BEFORE INSERT OR UPDATE OF user_id ON user_preferences
    FOR EACH ROW EXECUTE FUNCTION user_preferences_user_check();
