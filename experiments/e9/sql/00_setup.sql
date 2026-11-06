-- E9 setup: helper functions, field definitions, base issues (shared by A/B/C), raw (issue, field) pairs.
-- SPDX-License-Identifier: GPL-2.0-or-later
--
-- Determinism: every pseudo-random value is r(key, salt) = hashint8extended(key, seed-mixed salt),
-- so the data does not depend on plan order, parallel workers or setseed(). Seed is fixed in e9_seed().

CREATE FUNCTION e9_seed() RETURNS bigint LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$ SELECT 42::bigint $$;

-- uniform [0,1)
CREATE FUNCTION r(k bigint, salt bigint) RETURNS float8 LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT ((hashint8extended(k, salt * 1000003 + e9_seed()) >> 11) & ((1::bigint << 52) - 1))::float8
         / (1::bigint << 52)::float8
$$;

-- Deterministic, time-ordered (uuidv7-shaped) ids. IMMUTABLE so the planner constant-folds nid(123) etc.
CREATE FUNCTION nid(n bigint) RETURNS uuid LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT (lpad(to_hex(1730000000000 + n * 1000), 12, '0') || '7' || lpad(to_hex(n % 4096), 3, '0')
          || '8' || lpad(to_hex(n), 15, '0'))::uuid
$$;
CREATE FUNCTION pid(p int) RETURNS uuid LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT ('00000000-0000-7000-8000-' || lpad(to_hex(p), 12, '0'))::uuid $$;
CREATE FUNCTION uid(u int) RETURNS uuid LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT ('00000000-0001-7000-8000-' || lpad(to_hex(u), 12, '0'))::uuid $$;

-- Field definitions. 1..10 are "hot" (present on hot_p of issues); 11..1000 are drawn with a skewed
-- distribution (see cf_raw) so a handful are common and most are rare.
CREATE TABLE cf_defs (
    id     int PRIMARY KEY,
    ftype  text NOT NULL CHECK (ftype IN ('num', 'date', 'text', 'list', 'user', 'bool')),
    hot_p  float8 NOT NULL DEFAULT 0,   -- presence probability for hot fields
    card   int NOT NULL DEFAULT 0       -- number of distinct values for list/text
);
INSERT INTO cf_defs VALUES
  (1, 'num', 0.95, 21),     -- story points 0..20
  (2, 'date', 0.90, 0),     -- target date
  (3, 'list', 0.95, 5),     -- severity
  (4, 'user', 0.85, 500),   -- reviewer
  (5, 'bool', 0.90, 0),     -- regression?
  (6, 'text', 0.80, 2000),  -- component
  (7, 'list', 0.70, 12),    -- platform
  (8, 'num', 0.80, 0),      -- estimate (continuous, 2 decimals)
  (9, 'date', 0.60, 0),     -- found on
  (10, 'list', 0.60, 100);  -- customer
INSERT INTO cf_defs (id, ftype, card)
SELECT f,
       CASE WHEN u < 0.25 THEN 'num' WHEN u < 0.40 THEN 'date' WHEN u < 0.60 THEN 'text'
            WHEN u < 0.85 THEN 'list' WHEN u < 0.95 THEN 'user' ELSE 'bool' END,
       CASE WHEN u >= 0.40 AND u < 0.60 THEN 10 + floor(r(f, 2) * 5000)::int
            WHEN u >= 0.60 AND u < 0.85 THEN 3 + floor(r(f, 2) * 47)::int ELSE 0 END
FROM (SELECT f, r(f, 1) AS u FROM generate_series(11, 1000) f) s;

-- Base issue columns, same shape as migrations/0001_init.sql minus FKs to unrelated tables.
-- 200 projects with skewed sizes, 500 users, 10% private.
CREATE TABLE issues_base (
    n              int PRIMARY KEY,
    id             uuid NOT NULL,
    project_id     uuid NOT NULL,
    tracker_id     int NOT NULL,
    status_id      int NOT NULL,
    subject        text NOT NULL,
    author_id      uuid NOT NULL,
    assigned_to_id uuid,
    is_private     boolean NOT NULL,
    lock_version   int NOT NULL DEFAULT 0,
    created_at     timestamptz NOT NULL,
    updated_at     timestamptz NOT NULL
);
INSERT INTO issues_base
SELECT n, nid(n),
       pid(1 + floor(200 * r(n, 11) ^ 1.5)::int),
       1 + floor(r(n, 14) * 5)::int,
       1 + floor(r(n, 15) * 8)::int,
       'Issue ' || n || ' ' || left(md5(n::text), 24),
       uid(1 + floor(500 * r(n, 13))::int),
       CASE WHEN r(n, 16) < 0.7 THEN uid(1 + floor(500 * r(n, 17))::int) END,
       r(n, 12) < 0.10,
       0,
       timestamptz '2019-01-01 00:00+00' + n * interval '150 seconds',
       timestamptz '2019-01-01 00:00+00' + n * interval '150 seconds'
FROM generate_series(1, 1000000) n;

-- Which (issue, field) pairs have a value.
-- hot: Bernoulli(hot_p); cold: 4..18 draws per issue of field 11 + floor(990 * u^4), deduplicated.
CREATE UNLOGGED TABLE cf_raw AS
SELECT n, d.id AS f
FROM generate_series(1, 1000000) n CROSS JOIN cf_defs d
WHERE d.id <= 10 AND r(n::bigint * 16 + d.id, 21) < d.hot_p
UNION
SELECT n, 11 + floor(990 * r(n::bigint * 32 + j, 22) ^ 4)::int
FROM generate_series(1, 1000000) n
CROSS JOIN LATERAL generate_series(1, 4 + floor(r(n, 23) * 15)::int) j;
