# oxledger playbook

開發環境怎麼起、踩過的坑（症狀 → 原因 → 修法）。邊做邊補。

## 本機開發環境

需要：Rust stable（1.99+）、Docker、PostgreSQL 18（用容器）。

```bash
# PostgreSQL 18，只綁 localhost，資料在 volume oxledger-pgdata
docker run -d --name oxledger-pg -e POSTGRES_USER=oxledger -e POSTGRES_PASSWORD=oxledger_dev \
  -e POSTGRES_DB=oxledger -p 127.0.0.1:55432:5432 -v oxledger-pgdata:/var/lib/postgresql postgres:18

cp .env.example .env            # 或直接 export 裡面的變數
cargo build --features test-support   # seed-demo 只在這個 feature 下編譯（契約 02 §2 P5）
./target/debug/oxledger seed-demo   # 只能對空資料庫跑；印出 admin/alice/bob/carol 的 API key（只顯示這一次）
./target/debug/oxledger serve       # /api/v1（SPA，session cookie）、/mcp（Bearer）、/healthz
```

帳號與 agent（契約 02 §3，S1）：

```bash
echo 'a long password here' | ./target/debug/oxledger user add dave "Dave Wang" [--admin]   # 密碼從 stdin，至少 12 字元
echo 'another long one' | ./target/debug/oxledger user set-password alice   # 會結束 alice 所有 session
./target/debug/oxledger user lock|unlock bob
./target/debug/oxledger agent-client add "Claude Code" policy-2026-01       # 建立時是停用的
./target/debug/oxledger agent-client enable "Claude Code"
```

委派與 agent token 只能由**真人瀏覽器 session** 在 15 分鐘內重新驗證過密碼後建立（`POST /api/v1/delegations`、
`POST /api/v1/agent-contexts/{id}/credentials`），CLI 刻意不提供。手動試的流程見 `scripts/e2e_s1.py`：
`GET /auth/login-context`（拿 `__Host-ox_login` cookie 與 csrf_token）→ `POST /auth/login`（帶 `X-CSRF-Token`）→
之後非 GET 請求都要 `Content-Type: application/json`、`Origin: <OXLEDGER_SPA_ORIGIN>`、`X-CSRF-Token: <登入回傳的 csrf_token>`。

測試：`cargo test`（整合測試透過 dev-dependency 自動開 `test-support`）。整合測試（E4 可見性、自訂欄位、S0）預設用同一台 PostgreSQL 上的 `oxledger_test`；
`OXLEDGER_TEST_DATABASE_URL` 可以指到別的資料庫名稱，不存在會自動建（`tests/common/mod.rs`）。
多條工作線同時跑測試時各用各的，例如 `.../oxledger_test_d14`：測試會 TRUNCATE 整個 schema，共用會互相清掉資料。

### 接到 MCP client

- **HTTP（Streamable HTTP）**：URL `http://127.0.0.1:8080/mcp`，header `Authorization: Bearer <token>`。
- **stdio**：命令 `oxledger stdio`，環境變數 `DATABASE_URL`、`OXLEDGER_API_KEY`。log 走 stderr，不會污染 stdout。
- token 有兩種：agent token（`oxa_…`，由使用者在 SPA 為某個 agent client 發；只讀得到委派專案的非私有單與 `visible = true` 的欄位）
  和舊式 API key（`seed-demo` 印的那種，等同使用者本人的讀取權限，S3 起會限縮）。兩種都在每次工具呼叫重新檢查，撤銷後下一次呼叫就失敗。
- 示範資料的 AUTH 有 Reporter（own）角色，**不是受眾一致的專案**，不能委派給 agent（`project_not_audience_consistent`）。試 agent 要另建一個只有 all／default 角色的專案。

示範資料的權限（用來手動驗證可見性）：

| 使用者 | AUTH（私有專案） | WEB（公開專案） |
|---|---|---|
| admin | 全部（管理員） | 全部 |
| alice | Manager（all） | 非成員 → Non member（default） |
| bob | 透過群組 Backend 拿 Developer（default） | Non member |
| carol | Reporter（own） | Non member |

AUTH-5 是私有單，只有 admin、alice 看得到。

示範的自訂欄位（D14）：Severity（list，Bug）、Estimate (h)（float）、Target date（date，Feature／Task）、
Customer（string，只在 AUTH；受限欄位，只有 Manager 也就是 alice 和 admin 看得到）、Reviewer（user）、Regression（bool，Bug）、Root cause（text，不建索引）。
`search` 的 `filter` 例子：

```json
{"and": [{"field": "Severity", "op": "in", "value": ["High", "Critical"]},
         {"or": [{"field": "Estimate (h)", "op": "between", "value": [2, null]},
                 {"field": "Reviewer", "op": "eq", "value": "me"}]}]}
```

op：`eq` `ne` `lt` `le` `gt` `ge` `between`（兩端含，任一端可為 null）`in` `is_null` `not_null`。
排序比較與 `between` 只限 int／float／date；text 只能 `is_null`／`not_null`。
語意照 Redmine（`Query#sql_for_custom_field`）：欄位只在「適用」的單上算數，也就是 tracker 在欄位的 tracker 清單、
專案是 all 或在清單內，而且呼叫者在該專案讀得到這個欄位。不適用的單上就算存了舊值也不算。
`ne` 是 `eq` 的補集，所以會包含沒有值與不適用的單；`is_null` 只算適用但沒有值的單。
因此 `is_null` 不等於 `not(not_null)`。讀不到的欄位會回「no custom field」，和不存在的欄位一樣。

### SPA API（`/api/v1`，S2）

- 端點、request／response 型別：`api/openapi.json`（OpenAPI 3.1）與 `api/types.ts`（SPA 用），由 Rust DTO 產生：`oxledger api-spec`。
  改了 DTO 或路由後要重產，否則測試 `api_spec_is_current` 會失敗（也可以 `OXLEDGER_UPDATE_API_SPEC=1 cargo test --test s2_api` 直接覆寫）。
- 讀取：`POST /issues/search`（body 同 MCP `search` 的參數）、`GET /issues/{key}`、`POST /resolve`。REST 和 MCP 呼叫同一組 service（`registry.rs` 列出這些操作）。
- 分頁：回傳的 `next_cursor` 原樣帶回。cursor 綁定登入者、查詢條件與排序，24 小時有效（XChaCha20-Poly1305，金鑰存 `server_secrets`，啟動時載入；換金鑰：刪那一列並重啟全部 process）；預設每頁 50、最多 100；換條件、換人、竄改或過期都回 `invalid_cursor`，重新查一次即可。`limit` 可以每頁不同。

### 權限與 describe（S3）

- `GET /issues/{key}/actions`：這個人現在對這張單能做什麼（可改欄位、可轉的狀態與轉過去之後的必填／唯讀、note、duplicate）。`GET /projects/{id}/trackers/{tracker_id}/schema`：這個專案 × tracker 的欄位、型別、選項、可用的 op、建立規則與限制（`{id}` 可用專案 key、`{tracker_id}` 可用名稱）。MCP 是 `describe`，`scope` 為 `project_tracker` 或 `issue_actions`。兩者都不是授權票，寫入時一律重驗。
- `GET /projects/{id}/assignees`：可指派對象（有 `assignable` 角色的 active 使用者，含經群組取得的；群組本身要 `org_settings.issue_group_assignment = true`）。
- 權限名稱只能是 `perm::PERMISSIONS` 那 11 個（DB CHECK `role_permissions_known`），新增權限要兩邊一起改。
- 角色的 tracker 限制是**每個權限各自**的（`role_permission_trackers.permission`）；`view_issues` 全部 tracker、`edit_issues` 只有 Bug 是合法設定。

### 命令與里程碑（S4）

- `POST /commands`：人直接寫（human_direct）。header `Idempotency-Key` 必填（uuid）；同一個 key 重送回同一個結果（`execution_id` 相同），內容不同回 `idempotency_key_reused`，失敗不留紀錄（修好可以用同一個 key 再送）。文件格式見 `CommandDoc`（`api/types.ts`）：每張既有 issue 一定要有一條 `preconditions`（含 `lock_version`）。
- 一份文件改同一張單多次，`lock_version` 只加 1、outbox 一則、事件依 op 順序；全部 no_effect 時什麼都不寫，但仍回 `consistent_after`。
- 里程碑：`GET/POST /projects/{id}/versions`、`GET/PATCH/DELETE /versions/{id}`、`GET /versions/{id}/impact`、`GET /versions/{id}/events`。寫入要 `manage_versions` 與 `Idempotency-Key`；刪除是軟刪除，只有看得到的 issue 會擋。所有數字（stats、health、impact）只算呼叫者看得到的 issue。

### ChangeSet（agent 提案，S5）

- 流程：agent `propose`（S6 起經 MCP）→ 人在 `GET /changesets?bucket=awaiting_me` 看 → `POST /changesets/{id}/validations` 試算 → `POST /changesets/{id}/approve-and-apply`（帶 `version`、`proposal_hash`、`Idempotency-Key`）。同步套用：200 回 execution；結果未知時 202（`Location: /api/v1/executions/{id}`、`Retry-After: 2`、`client_action: retry_same_request`），用**同一個 key** 重送就好，不會套用兩次。
- 提案只寫 ChangeSet 表，不動 issue。step 9 的問題（workflow、lock_version 衝突）會存成 `blocked`／`conflict` 給人看；權限、委派、找不到這類拒絕不存。
- 寫入的署名：actor = agent（`actor_kind = agent`），approver／on_behalf_of = 批准的人；note 與新 issue 的作者是 agent、`created_on_behalf_of_id` = 委派人。
- 錯誤碼：舊版本 `version_superseded`、hash 不對 `approval_invalidated`、已套用 `already_applied`、已拒絕／取消 `changeset_not_executable`、過期 `changeset_expired`（410）、目標讀不到 `proposal_unavailable`。
- 測試用 `changesets::faults::arm(點, Action)`（只在 `test-support`）在交易 A／B／C 的邊界注入失敗、死結或閘門。

## 症狀 → 原因 → 修法

| 症狀 | 原因 | 修法 |
|---|---|---|
| `QueryBuilder<'_, Postgres>`：struct takes 0 lifetime arguments | sqlx 0.9 拿掉了 QueryBuilder 的 lifetime 參數 | 寫 `QueryBuilder<Postgres>` |
| `sqlx::query_as(&sql)`：`SqlSafeStr` is not implemented for `&String` | sqlx 0.9 要求動態 SQL 明確標示 | 確認字串沒有拼入使用者輸入後，用 `sqlx::AssertSqlSafe(sql)`；值一律用 bind |
| `query_scalar(sql)` 參數是 `&str` 時 borrowed data escapes | 同上，`&'static str` 才算安全字串 | 參數型別寫 `&'static str` |
| `no RoleServer in rmcp::model` | rmcp 3.9 的 `RoleServer` 在 crate root | `use rmcp::RoleServer;` |
| `Uuid: JsonSchema` not satisfied | rmcp 帶的 schemars 只開 `chrono04` | 自己加 `schemars = { version = "1", features = ["uuid1"] }`，feature 會合併 |
| `field tool_router is never read` | `#[tool_handler]` 預設每次呼叫 `Self::tool_router()` 重建 | `#[tool_handler(router = self.tool_router, ...)]` |
| `cargo test` 報 failed to remove `target\debug\oxledger.exe` | Windows 上執行中的 exe 被鎖住 | 先停掉 `oxledger serve`／`stdio` 再編譯 |
| 搜「登入」出現「首頁圖片載入太慢」 | 索引有單字（unigram），查詢「登入」的「入」命中「載入」 | 查詢端：兩字以上的中文串用 bigram（權重 1，`tokenize_query`），串裡的單字另以 `CJK_UNIGRAM_WEIGHT` = 0.1 加入（D15，`query_terms`），讓「付不了錢」這類 bigram 全空的查詢仍有結果；索引仍存兩種。E1 實測 nDCG＠10 0.781 → 0.810，權重 0.5 以上「載入」誤命中會回來，不要調高（見 `docs/research/03-e1-zh-retrieval.md`；測試 `login_does_not_rank_loading_issue_high` 在 0.5 會失敗） |
| 搜 `invalid_grant` 時 `invalid_request` 排得一樣前面；搜 `v3.2.0` 命中任何含 `2`、`0` 的 issue | 舊斷詞把 `_`、`.` 當分隔符，識別碼只剩零件 | 索引端在零件之外另存整串（`_`、`.` 連接時），查詢端整串權重 1、零件 `COMPOUND_PART_WEIGHT` = 0.2。改了斷詞要**重建索引**（`oxledger reindex`）。細節與取捨見下方「識別碼斷詞」 |
| 用 Python 讀 MCP stdio 輸出時 JSONDecodeError | Windows 預設以 cp950 解碼管線 | `io.TextIOWrapper(sys.stdin.buffer, encoding="utf-8")` |
| Docker 指令連不到 `dockerDesktopLinuxEngine` | Docker Desktop 沒在跑，或剛啟動 engine 還沒好 | 啟動 Docker Desktop，等 `docker info` 有 ServerVersion（約 1 分鐘） |
| 自訂欄位篩選在 generic plan 下全表掃描 `issue_cf` | 每欄位的 partial index 條件是 `WHERE field_id = 5`；`field_id = $1` 在 prepared statement 換成 generic plan 後，planner 無法證明它蘊含索引條件 | field id 直接以整數字面值寫進 SQL（`filter::push_filter`）。`tests/custom_fields_pg.rs` 用 `EXPLAIN (GENERIC_PLAN)` 驗證兩種寫法 |
| `EXPLAIN (GENERIC_PLAN) … $1` 回 `bind message supplies 0 parameters` | sqlx 的 `query` 走 extended protocol，會要求綁值 | 用 `sqlx::raw_sql`（simple protocol） |
| `CREATE INDEX CONCURRENTLY cannot run inside a transaction block`（或在函式裡） | CONCURRENTLY 不能在交易或函式內 | migration 只放產生 DDL 的函式（`cf_index_ddl`、`cf_drop_index_ddl`），由 `custom_fields.rs` 在交易外用 `sqlx::raw_sql` 執行 |
| 欄位索引建失敗後重跑仍沒有可用的索引 | 失敗的 `CREATE INDEX CONCURRENTLY` 留下 INVALID 索引，`IF NOT EXISTS` 會直接略過 | `build_index` 先查 `pg_index.indisvalid`，INVALID 就先 `DROP INDEX CONCURRENTLY` 再建；`reconcile_indexes` 會定期補 |
| 本分支的 binary 跑過共用的 `oxledger` 之後，其他分支的 binary 啟動失敗（未實際觸發，依 sqlx `Migrator` 預設行為推斷） | 資料庫記錄了 migration 0002，舊分支找不到這個檔案就拒絕執行 | 開發中的 migration 用獨立資料庫試，例如 `DATABASE_URL=.../oxledger_d14_demo` |
| 改了已套用的 migration 後，測試或 binary 啟動失敗：`migration 2 was previously applied but has been modified` | sqlx 會比對每個已套用 migration 的 checksum | 還沒進 main 的 migration 可以改，但要重建自己的測試／示範資料庫（例如 `DROP DATABASE oxledger_test_d14`，`test_pool` 會自動重建）；進 main 之後一律另開新 migration |
| 合成資料用 `generate_series … CROSS JOIN fields WHERE random() < coverage`，結果每個欄位不是全有就是全無 | planner 把 `random()` 的過濾放在只算一次的位置（實測 200 個欄位只長出 12 個，每個 10 萬筆） | 用決定性的 hash 抽樣：`hashint8extended(i * 1000 + field_id, seed)`，E9 也是這樣做 |
| 新增 migration 檔後測試還是舊行為（例如 0004 的 trigger 修正沒生效） | `sqlx::migrate!` 在編譯時嵌入 migrations；只新增檔案、Rust 原始碼沒變時 crate 不會重編 | `crates/oxledger/build.rs` 宣告 `rerun-if-changed=../../migrations`。不要刪這個檔 |
| 啟動或測試時 migration 0004 失敗：`N issue_cf row(s) do not match their custom field type` | 0002 的 `IF NOT ok` 讓檢查結果是 NULL 的值通過（例如 string 欄位寫進 `v_num`），0004 先掃描既有資料，有壞資料就拒絕繼續 | 列出來：`SELECT c.* , d.ftype FROM issue_cf c JOIN field_defs d ON d.id = c.field_id`，對照 0004 的 CASE 找出不符的列，搬到正確的值欄位或刪掉後重啟。訊息只列前 5 筆 |
| `cargo run -- seed-demo` 回「only in builds with the test-support feature」 | `seed`、`test_support` 直接寫表、不經命令引擎，正式建置不編進去 | `cargo run --features test-support -- seed-demo` |
| 用 curl 或 Python 打本機 http 的 /api/v1 時，登入後下一個請求還是 `unauthenticated` | cookie 名稱是 `__Host-` 前綴、`Secure`，http://127.0.0.1 上 cookie jar 會丟掉 | 手動帶 `Cookie:` header（`scripts/e2e_s1.py` 就是這樣）；瀏覽器在 localhost 會接受 Secure cookie |
| 登入或改密碼在 debug 建置要好幾秒、整合測試很慢 | Argon2id（19 MiB、t=2）在未最佳化的 debug 建置很慢 | workspace `Cargo.toml` 對 `argon2`、`blake2` 開 `opt-level = 3`（`[profile.dev.package.*]`），只影響 dev 建置 |
| `impl From<ApiError> for anyhow::Error` 編譯錯誤 conflicting implementations | anyhow 已有 `impl<E: std::error::Error + Send + Sync> From<E>` | 不要自己寫，直接 `.into()` 或 `Err(api(...))?` |
| clippy `collapsible_if` 指向巢狀 `if let` | Rust 2024 可以用 let chains | 改成 `if let A = a && let B = b { … }` |
| `POST /delegations` 回 `forbidden_op`／`project_not_audience_consistent` | 專案有 `own` 角色或 tracker 受限的 view_issues，agent 不能保證看到和每個人一樣的單 | 換專案，或改角色設定；這是刻意的（§3.5） |
| `POST /delegations` 回 `context_unavailable`（closed） | 使用者先前按過「停止這個 agent」，脈絡已關閉 | `POST /agent-contexts` 帶 `fresh_context_ack: true` 開新脈絡，舊 token 不會復活 |
| `api_spec_is_current` 失敗：api/openapi.json is stale | 改了 DTO、路由或 doc comment，沒有重產規格 | `cargo run -- api-spec`，把 `api/` 的變更一起 commit |
| 新增的 `/api/v1` 路由在 s2 測試報「documented but not routed」或反過來沒進 OpenAPI | 路由要在兩個地方登記：`api::router` 和 `openapi::session_routes`（或 `registry::tools` 的 `rest`） | 兩邊都加；s2 測試會對每條文件化的路由打一次請求 |
| `invalid_cursor`：伺服器重啟後舊 cursor 還能用嗎 | cursor 金鑰存在 `server_secrets`（第一次使用時產生），重啟與多個 process 共用；刪掉那列等於讓所有 cursor 失效 | 正常；client 收到 `invalid_cursor` 就從第一頁重查 |
| 路徑或文字參數含 NUL（`%00`）時回 500 | PostgreSQL text 不能存 NUL（SQLSTATE 22021） | `error::classify` 把 22021／22P05 當成 `invalid_request`；`get` 對格式不對的 key 直接回 `not_found` |
| describe 說某欄位 `workflow_readonly`，但那個角色明明沒有這條規則 | Redmine 演算法：該 tracker／狀態只要有任何欄位規則，讀不到的受限欄位對那個角色會補成 readonly；而且要「每個 workflow 角色都有規則」才生效（admin 用全部有 add/edit 的角色，常因某角色沒規則而不生效） | 看 `workflow_field_rules` 該 tracker × 狀態的所有角色列；這是刻意照 Redmine（`ox-core::workflow::field_rules` 的單元測試列了各種情況） |
| describe／get 沒有列出某個轉移 | 轉移要：可編輯（`edit_issues` 或作者的 `edit_own_issues`，且專案 active）、角色有 add/edit（consider_workflow）、條件成立（`author`／`assignee` 看交易開始時的值）；agent 另要委派含 `transition` 與 `status_id` | 對照 `describe::allowed_transitions` |
| `GET .../trackers/bug/schema` 回 `ambiguous_reference` | 有兩個 tracker 名稱只差大小寫 | 改用 tracker id |
| migration 0007 失敗：`role_permissions_known` | 既有 `role_permissions` 有清單外的權限名稱 | 先刪或改名那些列；匯入 Redmine 前要先處理 |
| 0007 之後有些 issue 的開始／到期日變成空的 | 0007 修復違反日期 CHECK 的舊資料（超出 1900–2199 清成 NULL；開始晚於到期兩端都清），原值與理由在那張 issue 的歷史（0008 起：`issue_events` kind `fields_changed`、`system_operation = 'u39_date_fix'`，payload 有 `changes` 與 `reasons`；`migration_date_fixes` 已刪） | 依事件裡的原值手動補回合理的值 |
| `POST /commands` 回 400 `invalid_request`，`details.kind = "idempotency_key"` | 沒帶 `Idempotency-Key` 或不是 uuid | 每次「新的操作」產生一個 uuid，重試時沿用同一個 |
| `POST /commands` 回 `invalid_request`「precondition」 | 每張既有 issue 要剛好一條 precondition（`lock_version` 必填）；新建的 temp issue 不能有 | 先 `GET /issues/{key}` 取 `lock_version` |
| `conflict`，`details.lock_version` 比送出的大 | 讀了之後有人改過這張單 | 重新讀、確認變更還適用，再用新的 `lock_version` 送 |
| 保留已停用的選項或已鎖定的版本卻回 `invalid_value`／`version_not_open` | 比較基準只看內建欄位時，自訂欄位原值會被當成新指定（S4 修過一次） | `check_write` 的 `orig` 要用 `Work.orig_values`（內建＋自訂）；`s4_commands` T-P6 會抓到 |
| `version_name_taken` 但名稱看起來不一樣 | `name_key` 是 NFC＋casefold：`V2.0`／`v2.0`、組合／分解的 `é` 都算同名 | 用 `details.version_id` 那個既有版本，或換名字 |
| 刪除版本回 `version_in_use`，但列出的單比實際少 | 只列呼叫者看得到的 issue（R5）；看不到的 issue 不會擋，刪除後它們保留指向已刪除的版本 | 刻意的；有權限的人看得到全部 |
| 建立專案後 INSERT `project_schedule_settings` 撞主鍵 | 0007 的 trigger 建立專案時已經插入一列 | 用 UPDATE，不要再 INSERT |
| 並行測試（兩個請求同時改）怎麼跑都過，拿掉鎖也過 | 兩個請求其實先後執行，沒有真的競爭 | 先用另一個交易持有上游的列（例如專案 `FOR UPDATE`），讓兩個請求都卡在同一點，再放開；`s4_versions` 的 M15 測試就是這樣 |
| agent 提案 `transition` 被當成 `invalid_transition` blocked，`allowed` 只剩目前狀態 | 委派的 `writable_fields` 沒有 `status_id`（transition 要 `transition` op 加 `status_id` 欄位） | 委派時加 `status_id` |
| approve 回 `approval_invalidated` | `proposal_hash` 不是目前版本的 hash（或 agent 已修訂出新版本） | 重新 `GET /changesets/{id}` 取 `version` 與 `proposal_hash` |
| 套用失敗後同一個 key 重送仍回 failed | 同 key 是 replay，回原結果 | 修好之後用**新的** key 再批准（attempt_no +1），或請 agent 修訂 |
| 測試要改 changeset 的 `expires_at` 卻報 immutable | `changesets_guard` 不讓應用程式改到期時間 | 測試裡用 `SET session_replication_role = replica` 暫停 trigger（`s5_changesets::set_expiry`） |
| 套用回 `delegation_unavailable`，但委派看起來還在 | agent client 被停用、委派到期或被撤銷，交易 B 會重查 | 重新啟用 client，或建立新委派後請 agent 重新提案 |
| 測試裡 agent 突然 `context_unavailable`（`kind = quarantined`），明明沒動 epoch | 0010 起，委派之後只要改了成員、角色、權限、欄位可見性、專案公開／狀態／模組、建立／停權使用者，相關專案（多數是全部）的 `security_epoch` 都會 +1 | 先把設定都做完再委派；要在委派後改設定，就 `agents::new_context` 開新脈絡再委派（`tests/s6_world` 的 `World::new_context`） |
| 關閉脈絡後舊 token 回 `context_unavailable`（`kind = closed`）而不是 `unauthenticated` | T-X20：脈絡關閉造成的失效要講清楚 | 刻意的；單獨撤銷的 token 才回 `unauthenticated` |
| 建專案卡住 | 另一個交易正在做全專案 epoch bump（持有 `project_catalog` FOR UPDATE） | 等那個交易提交；這是 T-X24 的協調 |
| agent 提案或寫入回 `forbidden_op`（`details.reason = "policy"`），但委派的 `proposal_ops` 明明有這個命令 | 0011 起，委派內每個命令、欄位都要有政策列；缺列＝off（§16.3 規則 1）。測試或資料修補直接 `INSERT INTO delegations` 時沒有一起建政策列 | 用 `agents::create_delegation`；直接複製委派時把 `delegation_command_policies`／`delegation_field_policies` 一起複製（`tests/s5_changesets.rs` 的 `clone_delegation`） |
| `writable_fields` 帶 `status_id` 回 `invalid_value` | 0011 起 `status_id` 不是欄位：狀態改變是 `transition` 命令 | 把 `transition` 放進 `proposal_ops`，`writable_fields` 拿掉 `status_id` |
| 測試裡 agent 寫入突然回 `agent_suspended`（`reason = audience_blocked`） | 熔斷：1 小時內 3 次 `audience_expansion_forbidden`（agent 層，跨脈絡）就暫停脈絡 | 測試刻意被擋時，之後清掉 `agent_write_log` 的 `audience_blocked` 列（`tests/s6_audience.rs` 的 `forbidden`）；正式環境由委派者在 SPA 解除 |
| 並行 `command_submit` 有些回 `rate_limited`（`kind = concurrent_write`） | 同一脈絡同時只允許 1 筆直接寫入（§16.6，控制列 NOWAIT） | 同一脈絡依序送；平行只能跨脈絡 |
| Python `subprocess.run(['cargo', …])` 在 Windows 報 FileNotFoundError | PATH 改了也不會影響 CreateProcess 找執行檔 | 用完整路徑 `~/.cargo/bin/cargo.exe` |
| rustup 裝完 `cargo` 找不到 | 安裝時用了 `--no-modify-path` | 指令前加 `export PATH="$HOME/.cargo/bin:$PATH"`，或自行把 `%USERPROFILE%\.cargo\bin` 加進 PATH |
| `utoipa` 產生 OpenAPI 時 panic：schema `Evidence` differs between request and response | 兩個不同的 struct 用了同一個 schema 名稱（commands 的 `Evidence` 是 propose 的輸入，search 的是輸出） | 改名，搜尋端叫 `SearchEvidence`；新增 DTO 前先 `grep -rn "pub struct 名稱"` |
| 在 CRLF 檔上用 perl 多行替換，沒有任何效果 | git `autocrlf=true`，檔案在磁碟上是 CRLF，pattern 裡的 `\n` 對不到 `\r\n` | 先 `perl -pi -e 's/\r\n/\n/'` 轉 LF，改完再轉回；或直接用編輯工具。perl 也不要用 `\|` 當分隔字元，會吃掉內容 |
| 搜尋結果的 `highlights` 和前端位置差一兩格 | 偏移是 Unicode code point（和 JS `Array.from` 一致），不是 UTF-8 byte，也不是 UTF-16 | 前端用 `Array.from(text).slice(start, end)`；後端用 `evidence::CpText` |
| 字素邊界在國旗 emoji（regional indicator 對）上切歪 | 沒有 unicode-segmentation crate，`evidence.rs` 只用近似的延伸字元清單（組合符號、ZWJ、VS、膚色、tag、CRLF） | 已知限制；要精確就加 unicode-segmentation（新 crate，需使用者同意） |
| agent 呼叫 get／get_many／verify_citations／get_source 得到 `delegation_unavailable`，但 search 回空結果 | 讀取的新工具經 `citations::Reader::new`，脈絡沒有任何有效委派就整批拒絕；search 仍是 S1 的行為 | 已知不一致，記在 decisions.tsv；先建立委派 |
| 測試中 `commands::submit` 回 `invalid_request: every existing target needs a precondition with lock_version` | 對既有 issue 的 op 一定要帶 precondition | 先讀目前 `lock_version` 再送（`tests/s6_read.rs` 的 `act`） |
| 全套測試偶爾 `s2_api` 第 548 行 ranked search 回 0 筆，單獨重跑 12 次都過 | `SearchIndex::sync` 只讀 `xid < pg_snapshot_xmin(pg_current_snapshot())` 的 outbox；xmin 是整個 PostgreSQL 叢集共用，同一台 server 上別的 DB（例如另一條線在跑測試）有長交易時，sync 這一輪會跳過剛寫入的 row，測試只 sync 一次就查 | 2026-11-08 證實：另一條線在同叢集跑測試時 `visibility_e4` 連續失敗（index 和參考結果不符），改用 `SearchIndex::sync_all`（test-support：重複 sync 直到 cursor 追上當下最後一筆 outbox）後，同樣情況連過 5 次。測試一律用 `sync_all` |
| agent 的 `changeset_get`／`changeset_list` 看得到 owner 修訂後改去的那張單（驗證者 B1） | 回應的 `targets` 從該版本的本文算出；owner_revise 版本的本文對 agent 是隱藏的，目標也是本文的一部分 | owner 版本一律回空 `targets`（`filter_for_agent`、`agent_list` 用 `is_agent_version` 判斷）；s6_mcp 斷言 |
| agent 的 cancel 沒有算進每委派 60 次／小時（驗證者 B3） | 只有 propose／revise 呼叫 `quota::record_proposal` | `changesets::cancel` 在 agent 路徑也 reserve＋record；s6_mcp 數 `agent_write_log` |
| agent 提議建立私有 issue、人核准後，agent 的 `changeset_get`／`operation_get` 拿得到提案本文（標題、欄位值）（T-M12） | 讀取檢查只看本文裡的既有目標；`create_issue` 沒有目標，建出來的 issue 讀不到也被當成「全部讀得到」 | `filter_for_agent` 把該版本 execution 的 `item_results` 裡的 issue（`created_issues`）也算目標；讀不到就不回本文，items 只回 `{op_id, status}`（key 只在讀得到時給） |
| 同一個脈絡委派 WEB 和 AUTH 後，WEB 的提案都被 `audience_expansion_forbidden`（cross_project） | 這是 §3.8 的規則：agent 可能讀過 AUTH，WEB 的讀者（carol）卻讀不到 AUTH | 測試要兩份委派時，先讓兩個專案讀者相同（carol 也加進 AUTH），再建脈絡；不是 bug |
| 帶 `consistent_after` 的搜尋一直回 `search_lagging` | frontier F 只在 index worker 跑過一輪後前進；別的 database 有已分配 xid 的未結束交易也會擋住 H（03 §4.7） | 測試用 `sync`／`sync_round` 迴圈等 `frontier() > X`（`tests/s8_search_consistency.rs` 的 `catch_up`）；正式環境看已分配 xid 的最老交易 |
| 兩個 session 同時跑測試，彼此的資料被清掉 | 測試開頭會 reset 整個測試 DB | 平行工作設 `OXLEDGER_TEST_DATABASE_URL` 指向各自的 DB（例如 `.../oxledger_test_main`），會自動建立 |
| 測試想把 `agent_write_log` 的時間往前改，觸發 `agent_write_log is append-only` | 0012 的 trigger 禁止 UPDATE／DELETE | 直接 INSERT 帶過去的 `at`（`clock_timestamp() - make_interval(secs => …)`） |
| 測試裡改了角色權限或成員之後，agent 的寫入回 `context_unavailable` | 0010 的 trigger 讓受影響專案 epoch +1，舊的工作脈絡失效（§3.8，正確行為） | 測試在權限／成員變更後重建脈絡：`new_context` → `delegate` → `token` |
| 測試連續做 10 筆以上 agent 直接寫入，第 11 筆回 `rate_limited` | §16.6 每分鐘 10 筆直接寫入（`DIRECT_PER_MINUTE`） | 測試把部分寫入改成「提案＋人核准」（changeset execution 也能 undo），或分散到不同測試 |
| agent 文件裡 `duplicate_of` 和其他 op 放一起回 invalid_request | `duplicate_of` 必須是文件唯一的 op（S4 規則） | 拆成兩份文件、兩筆 execution |
| 寫突變測試時，突變存活但其實有別層擋住（例如 revise 不檢查脈絡，仍因委派 id 不符被擋） | 深度防線 | 記成等價突變；要保護時改寫單元層斷言，不要只靠 E2E |
| 測試裡 `UPDATE roles SET issues_visibility` 之後，agent 寫入回 `context_unavailable`（quarantined） | roles 的 `issues_visibility` 有 epoch trigger，所有專案的 epoch 都會加一 | 改完角色就 `new_context` → `delegate` → `token` 重建 |
| 健康度門檻測試第二次跑結果不同（`at_risk_days` 是 5 不是 7） | `org_settings` 只有一列，測試的 reset 不清它 | 測試開頭與結尾都把 `health_at_risk_days`／`_open_pct` 設回 7／50 |
| 已過期的 open 版本 health 是 `on_track`，不是 `overdue` | `overdue` 要日期已過且還有 open issue（§17.12） | 測試要在版本裡放一張 open issue |
| `/issues/{key}` 的 not_found 兩次比對不相等 | 訊息回顯請求的 key | 比對前把 key 換成固定字串；其他欄位必須完全相同 |
| 測試 INSERT `project_setting_events` 失敗（FK） | `operation_request_id` NOT NULL 而且有 FK | 用 CTE 先 INSERT 一筆 `operation_requests`（fingerprint 32 bytes） |
| GitHub 測試第二次跑多出前一次的 PR 或 repo | GitHub 表不在通用 reset 範圍 | 測試開頭 `TRUNCATE github_* … CASCADE`（見 s6b_github_ingest `setup`） |
| 重抓（refetch）路徑的測試抓不到競態 | 假 API 回應前沒有並行寫入 | 假 API 的 `during_get` 在回應前送一筆較新的 webhook 並 drain，再回舊資料（T-G25／T-G35） |
| check_suite 一直 queued，PR 的 CI 永遠 pending | 別的 App 建立但從不執行的 suite | 只有本 head 有 run 引用的 suite 才算 CI 單位（decisions S6b.7） |
| `migration 16 was previously applied but has been modified` | 還沒發布的 migration 改過，測試 DB 存的是舊 checksum | 只在測試 DB：DROP 該 migration 建的表、`DELETE FROM _sqlx_migrations WHERE version = 16`，再跑測試；已發布的 migration 不可改，要另開新檔 |
| 全套測試沒有跑到 `s6b_github_sync` | 檔頭 `#![cfg(feature = "github")]` | 另跑 `cargo test -p oxledger --features test-support,github --test s6b_github_sync`（第一次要下載 TLS crate；之後 `--offline`） |
| mock 的 axum handler 編譯失敗（`Handler` not implemented） | `std::sync::MutexGuard` 跨 `.await`，future 不是 Send | 加鎖的部分寫成同步函式，回傳「要 await 的動作」，鎖放掉後才 await |
| client 測試清掉 mock 的 token 後一直 401 | 快取的 token 還沒到期 | client 遇到 401 會丟掉快取（decisions S6b.8）；token 生命週期的測試另建 client |
| 測試裡兩個 PR 的開發卡順序和預期相反 | 同 state、同 gh_updated_at 時依 PR id（uuidv7）排序 | 斷言照建立順序，或給 PR 不同的 updated_at |
| 屏障測試裡被擋的那一方提早完成 | fault 只在下一次經過時生效（`arm` 一次一發） | 先 `g.reached.notified().await` 再啟動另一方；放行前用 `is_finished()` 確認對方真的在等鎖 |
| 跨專案提案在測試裡回 audience_expansion_forbidden | 讀者集合不一致（U1）：目的地的讀者不是來源每個專案讀者的子集 | 測試讓兩專案受眾一致（同一批成員），或只往受眾較窄的專案提案 |

## 設計上的防呆

- **可見性只有一份規則**：`ox-core::Scope::predicate()` 產生 `Pred`，同時編成 SQL（`db::push_pred`）與 tantivy filter（`SearchIndex::filter`）。E4 測試拿 Redmine 規則的獨立實作對照四種結果。改規則時先跑 `cargo test -p oxledger --test visibility_e4`。
- **搜尋結果一律回 DB 複驗**：tantivy 可能落後資料庫（outbox 非同步），所以 `service::search` 會把候選 id 再過一次 SQL 可見性條件。
- **看不到 = 不存在**：`get` 對沒權限的 issue 回 `not_found`「no issue X」，和真的不存在一樣，避免用 key 探測。
- **身分只在一處檢查**（契約 02 §2 P4、§3.7）：`auth::AuthContext` 只記「是誰、用什麼憑證」（principal、method、session／credential／context id），欄位私有、只能由 `auth`／`api` 建立；能不能用一律由 `db::effective` 每次查 DB：session 沒撤銷、沒過期、密碼版本沒變；API key 沒撤銷沒過期；agent token 的 credential、脈絡、client、agent 與委派人都還有效，委派專案取當下沒撤銷沒過期的。`load_scope`、`role_ids_for`、`is_admin` 都先經過它。被鎖的使用者、撤銷的 token 在下一個請求就回 `unauthenticated`，長時間執行的 stdio 也一樣（S1 e2e 實測）。新增授權函式時一律從 `effective` 開始。
- **agent 的讀取範圍**（§3.5）：委派人的可見範圍 ∩ 委派專案 ∩ 非私有單 ∩ `visible = true` 欄位；委派人的 `own` 角色不帶給 agent。`load_scope` 對 agent 回 `Scope::anonymous` 形狀（只有非私有），`filter::resolve` 與 `service::custom_field_values` 對 agent 不算角色可讀性。
- **只有真人 session 能做的事**（T-A1）：委派、發 token、撤銷、開關脈絡在 service 層一開始就呼叫 `ctx.require_human_session()`，不靠路由擋；`/api/v1` 另外只收 cookie，帶 Bearer 一律 401、cookie 與 Bearer 並存 400。
- **step-up**：`auth::require_step_up` 在同一個交易裡對 session 加 `FOR SHARE` 再檢查 `reauthenticated_at`，所以「檢查完、寫入前被登出」不會成立。
- **鎖順序**（§3.7）：agent_work_contexts → principals → browser_sessions → delegations → agent_clients → projects。新寫的交易照這個順序拿鎖。
- **DB 層的身分防線**（T-A9，migration 0005）：agent principal 不能當成員、群組成員、指派對象、user 型自訂欄位值、API key 擁有者；principal 的 kind 不能改。這些是觸發器，繞過 Rust 直接寫 SQL 也擋得住。
- **agent 讀取每次重驗**：R 中任一專案 `security_epoch` 和授予時不同 → `context_unavailable`（quarantined），連讀取都停；委派專案若已不是受眾一致就當作沒委派。
- **step-up 不是密碼 oracle**：同一 session 錯 5 次密碼，session 直接結束。
- **管理員結束某人所有 session**：`oxledger user revoke-sessions <login>`（寫稽核）。
- **部署**：應用程式的 DB 帳號不要是 superuser（可停用觸發器防線）。
- **一次讀取一個 snapshot**（§3.7、§3.8 第 2 條）：`service::search`／`get`／`resolve` 先開 `read_tx`（READ ONLY、REPEATABLE READ），身分重驗、可見範圍、資料都在同一條連線、同一個 snapshot。內部函式收 `&mut PgConnection`；`filter::resolve` 的遞迴用 `tokio::sync::Mutex` 共用那條連線。
- **cursor 不帶授權**：每一頁都重新載入可見範圍；cursor 只記位置（keyset 的 `(updated_at, id)` 或全文排序的 offset），加密（XChaCha20-Poly1305）而不是只簽章，所以看不出內部 id。
- **登入速率限制在記憶體**：每個 login 15 分鐘 10 次失敗、每個 IP 50 次。多個 `serve` process 各算各的，要水平擴充時改成 DB 或共用存放。
- **授權函式吃連線，不吃 pool**（P3）：`load_scope`、`role_ids_for`、`is_admin`、`custom_fields::readable_projects` 收 `&mut PgConnection`。寫入路徑傳交易本身，權限判斷才會看到同一個快照與鎖；讀取路徑傳 `&mut app.pool.acquire().await?`。
- **錯誤碼**（P9，契約 02 §4.3、§4.4、§14.2）：可行動的錯誤是 `error::ApiError`（穩定的 `code`、`next_actions`、`details`）。MCP 回工具結果 `isError: true`，structured content 是錯誤物件本身；其他錯誤是 JSON-RPC internal error，只帶 `request_id`，原因只寫進 log。不要把 SQL、constraint 名稱放進 `message`。
- **測試用的直接寫入**（P5）：`test_support::create_issue` 不檢查權限、不走 workflow、不產生事件，只在 `test-support` feature 下編譯。
- **自訂欄位（D14，型別化 EAV）**：值在 `issue_cf`，每種型別一個值欄位；觸發器 `issue_cf_check` 保證值放在該型別的欄位（`int` 必須是整數、list 必須是該欄位的選項、user 必須存在、string／link 最多 2000 bytes 以免超過 btree 單筆上限）。空字串不存（`set_value("")` 等於清除），對齊 Redmine 把 '' 當成沒有值。篩選器依賴這些保證：每個條件都是一個 `EXISTS (… cf.field_id = <id> …)`，帶型別條件，也就是對 `v_x` 的比較，或 is_null／not_null 時的 `v_x IS NOT NULL`（報告 02 §5.3、§8）。外面再包一層適用性條件 E（tracker、專案、可讀專案），由 `db::push_pred` 編譯。
- **受限欄位（`field_defs.visible = false` + `field_roles`）**：只有 admin，以及在該專案的實際角色包含指定角色的人讀得到。實際角色同 `db::role_ids_for`：成員角色（含透過群組）；不是成員的公開專案用內建 Non member；沒登入用 Anonymous。`field_roles` 可以放內建角色（migration 0003）。`search`（filter，`custom_fields::readable_projects`）和 `get`（custom_fields）用同一套規則；改其中一邊要同步改另一邊，`custom_fields_pg` 的 Triage 欄位會抓到不一致。
- **欄位不能改型別**：有觸發器擋，Redmine 也不允許。
- **list 選項**：使用中的選項不能刪（觸發器檢查，且只接受 READ COMMITTED，原因寫在 migration 註解），要停用就設 `active = false`；寫入值時對選項加 `FOR KEY SHARE`，和刪除互斥。
- **每個欄位一個 covering partial index** `issue_cf_f<id> (field_id, v_x, issue_id) WHERE field_id = <id>`。建立欄位時同步建（`create_field` 等索引建完才回傳），刪欄位走 `drop_field`：先標成 `dropping`。寫入觸發器會對欄位定義加 `FOR SHARE`，所以這個 UPDATE 會等正在寫值的交易結束，之後新的寫入一律被拒。接著經索引刪值 → `DROP INDEX CONCURRENTLY` → 刪定義。`serve` 每 60 秒跑 `reconcile_indexes` 收拾中斷的建立或刪除。
- **`issue_cf.field_id` 沒有 FK**：刪欄位時 FK 檢查要用 field_id 查 `issue_cf`，只有該欄位的 partial index 能用，而它可能已經刪掉。改由觸發器檢查存在性並加鎖，`drop_field` 先刪值。
- **數字一律綁成 text 再 `::numeric`**：綁 `f64` 會變成 `numeric > float8`，PostgreSQL 會把欄位轉成 float8，索引就用不上（依型別規則推斷，沒實測）。
- **planner 低估列數：部分處理**（報告 02 §8 有實測數字）。比較條件不再多帶 `IS NOT NULL`，熱門數字範圍的低估從 22 倍降到 4.9 倍；`issue_cf_field_ref` 統計讓 list／user 等值從 30 倍修到約 1 倍。剩下的是數字和日期範圍的低估、is_null／not_null 的 4.75 倍低估，以及跨欄位或 join 的相關性。多個熱門欄位加 `LIMIT` 時的 join 順序沒有重測。
- **free text + 篩選**：tantivy 不知道自訂欄位、狀態和 assignee=none，所以 `q` 的候選從 500 開始，存活數不夠就擴到 2000、8000。到 8000 還不夠時回 `truncated: true`，表示可能還有更多。每輪都重新查索引，索引在查詢期間更新時排序可能變動（best effort）。
- **outbox 用 xid8**：只消費 `xid < pg_snapshot_xmin(pg_current_snapshot())` 的列；長交易會拖住索引更新，之後要加監控。

## 識別碼斷詞（2026-11-06，和 GPT 討論 2 輪後實作）

起因：E1 報告 §6 第 5 點，`_`、`.` 把錯誤碼和版本號拆散。規則寫在 `search.rs` 的 `Lexer`。

**規則**
- 非 CJK 的英數字串（Unicode `is_alphanumeric`，和原本的詞一樣）之間只隔一個 `_` 或 `.` 時，除了各段，再存一個完整的詞：`invalid_grant` → `invalid`、`grant`、`invalid_grant`；`v3.2.0` → `v3`、`2`、`0`、`v3.2.0`。
- 只取最長的那一串，不產生中間組合（`foo_bar.baz` 只多一個 `foo_bar.baz`）。
- 不會連接的情況：
  - 結尾的連接符：`invalid_grant.` 只產生 `invalid_grant`。
  - 開頭的連接符，或連續兩個連接符：`__init__` → `init`；`a..b`、`a._b` → `a`、`b`。
  - 中文是硬邊界：`登入.失敗` 不連接。
- 整串超過 128 bytes 時不存整串，各段當一般的詞。
- `-`、`/`、`:`、`@`、全形破折號仍是分隔符。
- 查詢端權重：整串 1、各段 0.2。各段的 0.2 沒有校準過，E1 的查詢集裡沒有識別碼查詢。

**討論中決定的事**
- `-` 先不連接，因為 `登入-失敗` 這類中文混寫，以及日期的語意都要另外想。`/`、`:`、`@`（路徑、URL、時間、email）也延後，等有實際需求再說。
- 版本號不特別處理（GPT 第 1 輪建議版本號只搜整串、不搜各段，第 2 輪改成和其他複合詞一樣）。代價是：
  - 搜 `v3.2` 而 issue 寫 `v3.2.0`，或搜 `3.2.0` 而 issue 寫 `v3.2.0` 時，沒有整串可以對上，只能靠 `v3`、`2`、`0` 這些零件，所以 `v3.9.2` 也可能排上來。這只能找回大致相關的結果，不懂版本前綴。
  - 之後若版本號查詢重要，做法是在索引端加有上限的前綴和去掉 `v` 的別名（`v3.2`、`3.2.0`），並寫專門的測試。
  - 「零命中才退回搜零件」不採用：只要有一筆不相關的命中，退回就不會觸發。
- 段落用 Unicode 英數字，不限 ASCII，和原本的詞一致；但不做正規化，全形 `ｖ３.２` 不等於 `v3.2`。
- 驗收：E1 的 nDCG＠10 不能低於 0.810，同時逐題看變化，並看索引大小。

**結果**
- E1（只跑 a、a2、b、c）：a 0.781、a+uni0.1 0.810，到小數第 3 位都沒變，recall／MRR／P＠10 也一樣。60 題裡只有 q01 降了 0.002。
- 索引：200 筆時 103.1 → 103.8 KB；10,000 筆時 1869.5 → 1880.4 KB（+0.6%）。
- E1 沒有識別碼查詢，所以精確度的提升只由測試證明：
  - `identifiers_glued_by_underscore_or_dot_are_kept_whole`：斷詞的邊界情況。
  - `exact_identifier_ranks_first`：只有零件命中的 issue，分數低於第一名的 0.15 倍；各段權重若改成 1.0，這個比例約 0.3，測試會失敗。
- 還沒做：真實或獨立撰寫的識別碼查詢集，包括版本號、相近錯誤碼、路徑、日期和中文邊界的反例。
