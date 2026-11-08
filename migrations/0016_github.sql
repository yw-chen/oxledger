-- SPDX-License-Identifier: GPL-2.0-or-later
-- Contract 02 S6b.7: GitHub integration (§19, U45), the data model of §19.3
-- as written in the contract, plus project_repo_links.paused_until (S6b.8). Development data is a read-time projection:
-- nothing here writes issues, issue_events or lock_version (R-G4).
--
-- Deliveries are kept 7 days and PRs/checks of repositories without any
-- link 30 days (§19.9); the sweeper does both. pr_issue_refs.issue_id
-- cascades with the issue (§19.5; T-G28 is a DB-level test, B15).

CREATE TABLE github_installations (
    id                 uuid PRIMARY KEY DEFAULT uuidv7(),
    gh_installation_id bigint NOT NULL UNIQUE,
    account_login      text NOT NULL,
    state              text NOT NULL CHECK (state IN ('active', 'suspended', 'deleted')),
    updated_at         timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE github_repos (
    id              uuid PRIMARY KEY DEFAULT uuidv7(),
    gh_repo_id      bigint NOT NULL UNIQUE,         -- 以 GitHub 數字 id 為準；改名、轉移不會斷
    installation_id uuid REFERENCES github_installations(id),   -- NULL＝App 已不能存取
    full_name       text NOT NULL,                  -- 顯示用，repository 事件更新
    is_private      boolean NOT NULL,
    linkable        boolean NOT NULL DEFAULT false, -- 系統管理員開放
    state           text NOT NULL CHECK (state IN ('accessible', 'inaccessible')),
    updated_at      timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE project_repo_links (
    id          uuid PRIMARY KEY DEFAULT uuidv7(),
    project_id  uuid NOT NULL REFERENCES projects(id),
    repo_id     uuid NOT NULL REFERENCES github_repos(id),
    linked_by   uuid NOT NULL REFERENCES principals(id),
    linked_at   timestamptz NOT NULL DEFAULT now(),
    unlinked_at timestamptz,
    unlinked_by uuid REFERENCES principals(id),
    sync_state  text NOT NULL DEFAULT 'backfilling' CHECK (sync_state IN ('backfilling', 'ok', 'paused')),
    synced_through timestamptz,                  -- 對帳水位：這個時間以前更新的 PR 都已確認；NULL＝回填未完成
    paused_until timestamptz,                    -- 不在契約 DDL：rate limit 的 reset 時間（sync_state = paused 時），S6b.8
    CHECK ((unlinked_at IS NULL) = (unlinked_by IS NULL))
);
CREATE UNIQUE INDEX project_repo_links_active_uq ON project_repo_links (project_id, repo_id) WHERE unlinked_at IS NULL;

CREATE TABLE github_pull_requests (
    id            uuid PRIMARY KEY DEFAULT uuidv7(),
    repo_id       uuid NOT NULL REFERENCES github_repos(id),
    number        integer NOT NULL,
    title         text NOT NULL,                  -- 截到 256 字
    head_ref      text NOT NULL,
    head_sha      text NOT NULL,
    state         text NOT NULL CHECK (state IN ('open', 'draft', 'merged', 'closed')),
    review_state  text NOT NULL CHECK (review_state IN ('none', 'pending', 'approved', 'changes_requested')),
    ci_state      text CHECK (ci_state IN ('pending', 'success', 'failure')),   -- NULL＝沒有任何 check
    html_url      text NOT NULL,
    gh_updated_at timestamptz NOT NULL,          -- 亂序保護用（見 R-G8）
    content_hash  bytea NOT NULL,                -- 存下欄位的 hash；同時間戳不同內容時觸發重抓
    row_version   bigint NOT NULL DEFAULT 0,     -- 本地每次寫入 +1；重抓提交前比對
    UNIQUE (repo_id, number)
);
CREATE TABLE github_check_runs (              -- 每個 check_run 一列，以 GitHub 的 run id 為鍵
    gh_run_id    bigint PRIMARY KEY,
    repo_id      uuid NOT NULL REFERENCES github_repos(id),
    head_sha     text NOT NULL,
    app_id       bigint NOT NULL,                -- 產生這個 check 的 GitHub App
    check_suite_id bigint,                       -- GitHub 的 check suite（Actions：一個 workflow run 一個 suite）；缺值＝不能判斷屬於哪個序列
    name         text NOT NULL,
    status       text NOT NULL CHECK (status IN ('pending', 'completed')),   -- GitHub 的 queued／in_progress／waiting／requested／pending 一律正規化成 pending；未知的非 completed 值也當 pending
    gh_status    text NOT NULL,                  -- GitHub 原值，顯示用
    conclusion   text,                           -- completed 時才有
    started_at   timestamptz, completed_at timestamptz
);
CREATE INDEX github_check_runs_sha_idx ON github_check_runs (repo_id, head_sha);
CREATE TABLE github_check_suites (            -- GitHub 自己算的 suite 結論（含重跑後的更新）
    gh_suite_id bigint PRIMARY KEY,
    repo_id     uuid NOT NULL REFERENCES github_repos(id),
    head_sha    text NOT NULL,
    app_id      bigint NOT NULL,
    status      text NOT NULL CHECK (status IN ('pending', 'completed')),   -- 非 completed 一律 pending
    conclusion  text,
    gh_updated_at timestamptz NOT NULL,
    content_hash  bytea NOT NULL,
    row_version   bigint NOT NULL DEFAULT 0
);
CREATE INDEX github_check_suites_sha_idx ON github_check_suites (repo_id, head_sha);
CREATE TABLE github_commit_statuses (         -- 舊式 status API：每個 context 取 id 最大的那筆
    repo_id      uuid NOT NULL REFERENCES github_repos(id),
    head_sha     text NOT NULL,
    context      text NOT NULL,
    gh_status_id bigint NOT NULL,                -- GitHub 對同一 sha＋context 的 id 遞增
    state        text NOT NULL CHECK (state IN ('pending', 'success', 'failure', 'error')),
    PRIMARY KEY (repo_id, head_sha, context)
);
CREATE TABLE project_pr_hidden (              -- 在某個專案隱藏某個 PR（只影響該專案）
    project_id uuid NOT NULL REFERENCES projects(id),
    pr_id      uuid NOT NULL REFERENCES github_pull_requests(id) ON DELETE CASCADE,
    hidden_by  uuid NOT NULL REFERENCES principals(id),
    hidden_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (project_id, pr_id)
);
CREATE TABLE pr_issue_refs (                  -- 卡號比對結果
    pr_id      uuid NOT NULL REFERENCES github_pull_requests(id) ON DELETE CASCADE,
    issue_id   uuid NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    project_id uuid NOT NULL,                   -- = issue 的專案（不可改，§1）
    link_id    uuid NOT NULL REFERENCES project_repo_links(id),   -- 建這筆 ref 時依據的連動；讀取時 join 確認仍有效
    sources    text[] NOT NULL,                 -- {title, branch, commit}
    first_seen timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (pr_id, issue_id)
);
CREATE INDEX pr_issue_refs_issue_idx ON pr_issue_refs (issue_id);
CREATE TABLE github_deliveries (              -- 去重與重放保護，保留 7 天
    delivery_id  uuid PRIMARY KEY,              -- X-GitHub-Delivery
    event        text NOT NULL,
    received_at  timestamptz NOT NULL DEFAULT now(),
    processed_at timestamptz,
    result       text                           -- ok／ignored／error 碼；不存 payload 原文
);
CREATE TABLE github_event_jobs (              -- 待處理事件；和 delivery 同交易寫入
    delivery_id  uuid PRIMARY KEY REFERENCES github_deliveries(delivery_id),
    payload      jsonb,                         -- 處理成功或最終失敗後設 NULL
    attempts     smallint NOT NULL DEFAULT 0,
    next_at      timestamptz NOT NULL DEFAULT now()
);
