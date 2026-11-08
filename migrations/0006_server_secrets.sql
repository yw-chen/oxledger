-- SPDX-License-Identifier: GPL-2.0-or-later
-- Contract 02 S2 (§4.5): server-side secrets, first the cursor encryption key.
-- Created on first use by the application (32 random bytes), so every server
-- process sharing the database encrypts and decrypts cursors with the same key.
-- Each process caches the key from startup. To rotate: DELETE the row, then
-- restart every server process (a process still running keeps the old key, and
-- processes with different keys reject each other's cursors). Outstanding
-- cursors become invalid_cursor; clients handle that by searching again.

CREATE TABLE server_secrets (
  name       text PRIMARY KEY CHECK (name ~ '^[a-z_]+$'),
  secret     bytea NOT NULL CHECK (length(secret) = 32),
  created_at timestamptz NOT NULL DEFAULT now()
);
