-- SPDX-License-Identifier: GPL-2.0-or-later
-- Design 05 M1.10 (§2.12, §3.13, A4, K11): the built-in AI chat in the
-- right-hand column. Site settings and budgets, providers that point at
-- endpoints registered in the deployment (never a URL or a key in the
-- database, Q1), conversations, messages, runs (lease, prompt version,
-- time taint), daily and monthly usage, the built-in agent client
-- `oxledger-ai`, and the `builtin_ai` changeset origin reserved by 0009.
--
-- No role, permission or membership changes and no principal is inserted,
-- deleted or re-statused: security_epoch does not move.
--
-- Differences from the §2.12 draft (docs/impl/decisions.tsv M1.10-*):
--   * ai_runs.cancel_requested_at: a cancel reaching another process than
--     the one running the run is seen at its next step (§3.13.4 cancel);
--   * ai_providers.config_changed_at: "last_test_ok_at later than the last
--     configuration change" (§3.13.2) needs the change time apart from
--     updated_at (a test also updates the row);
--   * the 0009 CHECK (origin = 'external_agent') is found by its definition
--     instead of an assumed automatic name;
--   * an existing client named oxledger-ai that is already marked builtin
--     (only test databases of M1.9 have one) is adopted instead of failing;
--   * user_column_guard on ai_runs.user_id and ai_usage_daily.user_id too
--     (the draft names ai_conversations.user_id only).

-- ---------------------------------------------------------------- settings

CREATE TABLE ai_settings (
    id                          boolean PRIMARY KEY DEFAULT true CHECK (id),
    enabled                     boolean NOT NULL DEFAULT false,
    active_provider_id          uuid,
    monthly_budget_usd          numeric(10,2) CHECK (monthly_budget_usd IS NULL OR monthly_budget_usd > 0),
    user_daily_budget_usd       numeric(8,2)  NOT NULL DEFAULT 2.00 CHECK (user_daily_budget_usd > 0),
    user_runs_per_hour          integer NOT NULL DEFAULT 30 CHECK (user_runs_per_hour BETWEEN 1 AND 600),
    conversation_retention_days integer NOT NULL DEFAULT 30 CHECK (conversation_retention_days BETWEEN 1 AND 365),
    lock_version                integer NOT NULL DEFAULT 0,
    updated_by                  uuid REFERENCES principals(id),
    updated_at                  timestamptz NOT NULL DEFAULT now(),
    CHECK (NOT enabled OR (active_provider_id IS NOT NULL AND monthly_budget_usd IS NOT NULL))
);
INSERT INTO ai_settings DEFAULT VALUES;

-- Q1: the endpoint and its key are registered by the deployment
-- (OXLEDGER_AI_ENDPOINT_<NAME>, OXLEDGER_AI_KEY_<NAME>); the database only
-- names the registration.
CREATE TABLE ai_providers (
    id                      uuid PRIMARY KEY DEFAULT uuidv7(),
    name                    text NOT NULL UNIQUE CHECK (char_length(name) BETWEEN 1 AND 60),
    kind                    text NOT NULL CHECK (kind IN ('anthropic', 'openai_compat')),
    endpoint_name           text NOT NULL CHECK (endpoint_name ~ '^[A-Z][A-Z0-9_]{0,39}$'),
    model                   text NOT NULL CHECK (char_length(model) BETWEEN 1 AND 200),
    max_output_tokens       integer NOT NULL DEFAULT 4096 CHECK (max_output_tokens BETWEEN 256 AND 32000),
    input_usd_per_mtok      numeric(10,4) NOT NULL CHECK (input_usd_per_mtok >= 0),
    output_usd_per_mtok     numeric(10,4) NOT NULL CHECK (output_usd_per_mtok >= 0),
    supports_tools          boolean NOT NULL DEFAULT false,
    last_test_at            timestamptz,
    last_test_ok_at         timestamptz,
    last_test_result        jsonb,
    d17_attested_by         uuid NOT NULL REFERENCES principals(id),
    d17_attested_at         timestamptz NOT NULL,
    org_account_attested_by uuid REFERENCES principals(id),
    org_account_attested_at timestamptz,
    lock_version            integer NOT NULL DEFAULT 0,
    created_at              timestamptz NOT NULL DEFAULT now(),
    updated_at              timestamptz NOT NULL DEFAULT now(),
    config_changed_at       timestamptz NOT NULL DEFAULT now(),
    retired_at              timestamptz,
    CHECK ((org_account_attested_by IS NULL) = (org_account_attested_at IS NULL))
);
ALTER TABLE ai_settings ADD FOREIGN KEY (active_provider_id) REFERENCES ai_providers(id);

CREATE TABLE ai_provider_events (
    id          uuid PRIMARY KEY DEFAULT uuidv7(),
    provider_id uuid REFERENCES ai_providers(id),
    at          timestamptz NOT NULL DEFAULT now(),
    actor_id    uuid NOT NULL REFERENCES principals(id),
    kind        text NOT NULL CHECK (kind IN ('created','updated','retired','activated','settings_updated','test_run')),
    payload     jsonb NOT NULL
);
CREATE INDEX ai_provider_events_at_idx ON ai_provider_events (at DESC);

-- ---------------------------------------------------------------- the built-in client

DO $$ BEGIN
    IF EXISTS (SELECT 1 FROM agent_clients WHERE name = 'oxledger-ai' AND NOT builtin) THEN
        RAISE EXCEPTION 'agent_clients already has a client named oxledger-ai; rename it before upgrading (see docs/impl/upgrade-notes.md)';
    END IF;
END $$;
INSERT INTO agent_clients (name, enabled, egress_policy_ref, builtin)
    VALUES ('oxledger-ai', false, 'ai_settings.active_provider_id', true)
    ON CONFLICT (name) DO UPDATE SET enabled = false, egress_policy_ref = EXCLUDED.egress_policy_ref;

-- ---------------------------------------------------------------- builtin_ai proposals

DO $$
DECLARE c text;
BEGIN
    FOR c IN SELECT conname FROM pg_constraint
             WHERE conrelid = 'changesets'::regclass AND contype = 'c'
               AND pg_get_constraintdef(oid) = 'CHECK ((origin = ''external_agent''::text))'
    LOOP
        EXECUTE format('ALTER TABLE changesets DROP CONSTRAINT %I', c);
    END LOOP;
END $$;
ALTER TABLE changesets
    ADD CONSTRAINT changesets_origin_agent_ck CHECK (origin IN ('external_agent', 'builtin_ai')),
    ADD CONSTRAINT changesets_builtin_ctx_ck CHECK (origin <> 'builtin_ai' OR (delegation_id IS NOT NULL AND context_id IS NOT NULL));

-- ---------------------------------------------------------------- conversations

CREATE TABLE ai_conversations (
    id                 uuid PRIMARY KEY DEFAULT uuidv7(),
    user_id            uuid NOT NULL REFERENCES principals(id),
    agent_principal_id uuid NOT NULL,
    runtime_id         uuid NOT NULL,
    context_id         uuid NOT NULL,
    title              text NOT NULL DEFAULT '' CHECK (char_length(title) <= 120),
    state              text NOT NULL DEFAULT 'open' CHECK (state IN ('open','read_only')),
    last_provider_id   uuid REFERENCES ai_providers(id),
    -- The provider's config_changed_at at the last send: a new endpoint or
    -- model of the same provider needs the user's confirmation too (Q4).
    last_provider_config_at timestamptz,
    created_at        timestamptz NOT NULL DEFAULT now(),
    last_at            timestamptz NOT NULL DEFAULT now(),
    purge_after        timestamptz NOT NULL,
    purged_at          timestamptz,
    deleted_at         timestamptz,
    runtime_kind       text NOT NULL DEFAULT 'builtin' CHECK (runtime_kind = 'builtin'),
    FOREIGN KEY (agent_principal_id, user_id) REFERENCES agent_principals (agent_principal_id, delegator_user_id),
    FOREIGN KEY (agent_principal_id, runtime_id) REFERENCES agent_principals (agent_principal_id, runtime_id),
    FOREIGN KEY (context_id, runtime_id) REFERENCES agent_work_contexts (id, runtime_id),
    FOREIGN KEY (runtime_id, runtime_kind) REFERENCES agent_runtimes (id, kind)
);
CREATE INDEX ai_conversations_user_idx ON ai_conversations (user_id, last_at DESC) WHERE deleted_at IS NULL;
CREATE INDEX ai_conversations_purge_idx ON ai_conversations (purge_after) WHERE purged_at IS NULL;
CREATE TRIGGER ai_conversations_user_guard BEFORE INSERT OR UPDATE OF user_id ON ai_conversations
    FOR EACH ROW EXECUTE FUNCTION user_column_guard('user_id');

CREATE TABLE ai_messages (
    id              uuid PRIMARY KEY DEFAULT uuidv7(),
    conversation_id uuid NOT NULL REFERENCES ai_conversations(id) ON DELETE CASCADE,
    run_id          uuid,
    seq             integer NOT NULL,
    role            text NOT NULL CHECK (role IN ('user','assistant','tool')),
    parts           jsonb,
    source_projects uuid[] NOT NULL DEFAULT '{}',
    time_tainted    boolean NOT NULL DEFAULT false,
    page_context    jsonb,
    created_at      timestamptz NOT NULL DEFAULT now(),
    purged_at       timestamptz,
    UNIQUE (conversation_id, seq),
    CHECK ((parts IS NULL) = (purged_at IS NOT NULL))
);
CREATE INDEX ai_messages_source_idx ON ai_messages USING gin (source_projects) WHERE purged_at IS NULL;

-- ---------------------------------------------------------------- runs

CREATE TABLE ai_runs (
    id                  uuid PRIMARY KEY DEFAULT uuidv7(),
    conversation_id     uuid REFERENCES ai_conversations(id) ON DELETE SET NULL,
    user_id             uuid NOT NULL REFERENCES principals(id),
    provider_id         uuid NOT NULL REFERENCES ai_providers(id),
    model               text NOT NULL,
    prompt_version      integer NOT NULL,
    state               text NOT NULL CHECK (state IN ('queued','running','succeeded','failed','canceled')),
    stop_reason         text CHECK (stop_reason IN ('end_turn','max_tokens','tool_limit','deadline','budget_exhausted',
                                                    'canceled','provider_error','context_unavailable','ai_unavailable',
                                                    'interrupted')),
    lease_until         timestamptz,
    cancel_requested_at timestamptz,
    time_tainted        boolean NOT NULL DEFAULT false,
    tool_calls          integer NOT NULL DEFAULT 0,
    input_tokens        integer NOT NULL DEFAULT 0,
    output_tokens       integer NOT NULL DEFAULT 0,
    reserved_usd        numeric(10,6) NOT NULL DEFAULT 0,
    cost_usd            numeric(10,6) NOT NULL DEFAULT 0,
    usage_estimated     boolean NOT NULL DEFAULT false,
    execution_ids       uuid[] NOT NULL DEFAULT '{}',
    changeset_ids       uuid[] NOT NULL DEFAULT '{}',
    started_at          timestamptz NOT NULL DEFAULT now(),
    finished_at         timestamptz,
    CHECK ((state IN ('succeeded','failed','canceled')) = (finished_at IS NOT NULL)),
    CHECK ((state IN ('queued','running')) = (lease_until IS NOT NULL))
);
ALTER TABLE ai_messages ADD FOREIGN KEY (run_id) REFERENCES ai_runs(id);
CREATE INDEX ai_messages_run_idx ON ai_messages (run_id);
CREATE INDEX ai_runs_user_started_idx ON ai_runs (user_id, started_at);
CREATE INDEX ai_runs_lease_idx ON ai_runs (lease_until) WHERE state IN ('queued','running');
CREATE UNIQUE INDEX ai_runs_one_active ON ai_runs (user_id) WHERE state IN ('queued','running');
CREATE TRIGGER ai_runs_user_guard BEFORE INSERT OR UPDATE OF user_id ON ai_runs
    FOR EACH ROW EXECUTE FUNCTION user_column_guard('user_id');

-- ---------------------------------------------------------------- budgets

CREATE TABLE ai_budget_months (
    month        date PRIMARY KEY CHECK (extract(day FROM month) = 1),
    reserved_usd numeric(12,6) NOT NULL DEFAULT 0,
    spent_usd    numeric(12,6) NOT NULL DEFAULT 0,
    notified_80  boolean NOT NULL DEFAULT false,
    notified_100 boolean NOT NULL DEFAULT false
);
CREATE TABLE ai_usage_daily (
    user_id       uuid NOT NULL REFERENCES principals(id),
    day           date NOT NULL,
    runs          integer NOT NULL DEFAULT 0,
    input_tokens  bigint NOT NULL DEFAULT 0,
    output_tokens bigint NOT NULL DEFAULT 0,
    reserved_usd  numeric(10,6) NOT NULL DEFAULT 0,
    spent_usd     numeric(10,6) NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, day)
);
CREATE TRIGGER ai_usage_daily_user_guard BEFORE INSERT OR UPDATE OF user_id ON ai_usage_daily
    FOR EACH ROW EXECUTE FUNCTION user_column_guard('user_id');
