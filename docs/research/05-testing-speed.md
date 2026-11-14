# 05 測試速度與可靠度：現況瓶頸與業界作法

日期：2026-11-13　範圍：後端整合測試（crates/oxledger/tests）、前端 vitest 與 Playwright E2E
性質：研究報告，沒有改任何程式、沒有安裝任何工具、沒有跑任何測試（只對已編好的測試 binary 做 `--list`）。

---

## 1. 結論先講

1. **慢的主因不是測試本身，是「每個測試都重建世界」再乘上「只能一個一個跑」。** 402 個整合測試裡有 379 次呼叫 `test_pool()`、233 處 `World::build`／`reset`／`TRUNCATE`。每次都要開兩次連線、檢查 31 個 migration、`TRUNCATE` 約 50 張表 `CASCADE`、`reconcile_indexes`，再用 Argon2（19 MiB）建 3 個使用者並登入 1 次。20–30 分鐘除以 402 個測試，平均每個 3–4.5 秒，大部分應該花在這段準備上（推估，見 §2.4 的量測建議）。
2. **必須單執行緒的原因只有一個：所有測試共用 `oxledger_test` 一個資料庫，開頭就 `TRUNCATE` 整個 schema。** 兩個測試同時跑就互相清掉資料（p1-backend-gaps.md G18 已記錄 `issue_statuses_name_key` 重複）。
3. **G21 類逾時的根因比「負載高」更具體：** 搜尋索引與通知 worker 的前進界線 H 是 `pg_snapshot_xmin(pg_current_snapshot())`，這是**整個 PostgreSQL cluster 共用**的值。同一台 `oxledger-pg` 上任何一個資料庫（開發伺服器、E2E、別條工作線的測試庫）有已分配 xid 的未結束交易，就會卡住這裡的 H，`sync_all` 只能空轉到 20 秒逾時。所以「改成每個測試一個資料庫」**不會自動解決 G21，平行之後反而會更常發生**，必須一起處理（§4.4）。
4. **建議導入順序：** (1) 每個測試從 template database 複製一個獨立資料庫＋專用的測試 PostgreSQL（關 fsync、和開發庫分開）＋平行；(2) 把 70 個測試 binary 合成 1 個；(3) 導入 cargo-nextest（需要你同意安裝），用 test group 隔離會長時間開交易的測試、只對已知 flaky 的測試開重試，並做「依 git diff 選測試」的腳本。預估全套從 20–30 分鐘降到 3–6 分鐘（推估，需實測）。

---

## 2. 現況量測與瓶頸

### 2.1 量測數字（唯讀取得）

| 項目 | 數值 | 來源 |
|---|---|---|
| `tests/` 下的測試檔 | 70 個 `.rs`（另有 `common/`、`s6_world/`、`ai_world/`、`ai_fake/`、`github_mock/` 共用模組） | `ls` |
| 整合測試數 | 402（`#[tokio::test]` 393、multi_thread 3、`#[test]` 6） | `grep` 屬性 |
| `src/` 內單元測試數 | 116 | `grep` |
| 測試程式總行數 | 68,528 行 | `wc -l` |
| 已編好的測試 `.exe` | 68 個，合計 2.26 GB；單一個 28–60 MB（`s6_read` 27.7 MB、`m1_11_gate` 60 MB） | `target/debug/deps` |
| 抽樣 `--list` | `s6_read` 10、`m1_11_gate` 13、`s8_search_consistency` 3 | 直接執行 exe `--list` |
| 每個測試呼叫 `test_pool()` | 379 次 | `grep` |
| 每個測試重建世界（`World::build`／`reset`／`TRUNCATE`） | 233 處；14 個檔各有一份自己的 `TRUNCATE` 清單 | `grep` |
| 固定 sleep（非輪詢） | `s4_commands` 3.0 s、`m1_10_ai_run` 2.5 s、`m1_4_time` 1.5 s×2、`p2_search` 1.2 s、`m1_2_views`／`m1_9_ai_team` 0.5 s… 合計約 15 s | `grep sleep` |
| 機器 | 12 個邏輯核心，Windows 11，PostgreSQL 18 在 Docker（127.0.0.1:55432） | `nproc` |
| 前端單元測試 | `src/**/*.test.ts` 只有 2 個檔 | `find` |
| 前端 E2E | 1 個 spec 檔、18 個測試，`workers: 1`、`fullyParallel: false` | `web/playwright.config.ts` |

沒做的量測：單一測試的準備時間拆解（`test_pool` vs `TRUNCATE` vs Argon2）。這需要實際跑測試，任務限制不跑，列為 A1 問題。

### 2.2 為什麼必須 `--test-threads=1`

- `tests/common/mod.rs` 的 `test_pool()` 永遠連到同一個 `oxledger_test`（或 `OXLEDGER_TEST_DATABASE_URL` 指的那一個）。
- `s6_world::reset` 一開始就 `TRUNCATE` 約 50 張表 `CASCADE`，其他 13 個檔也各有自己的 `TRUNCATE`。
- `cargo test` 預設在同一個 binary 內用多執行緒跑測試，兩個測試同時 `TRUNCATE`／insert 相同名稱的 `issue_statuses` 就撞 unique key（G18）。
- 不同 binary 之間 `cargo test` 本來就是一個接一個跑，所以真正的衝突在 binary 內；但只要改用 nextest 或 binary 合併，跨檔也會同時跑，問題一樣。

### 2.3 為什麼需要 `RUST_MIN_STACK=16777216`

repo 裡沒有寫原因（只在 `docs/impl/m1-11-e5.md` 的跑法出現，來自 commit ccc5a6d M1.11）。推測是 debug 建置下 async 狀態機很大（`commands::submit`、`World::build` 這類大函式的 future 疊在一起），Rust 測試執行緒預設 2 MiB 的堆疊不夠而 stack overflow。這不影響速度，但每次都要手動設環境變數是可靠度問題：

- 低成本作法：寫進 `.cargo/config.toml` 的 `[env]`，或在 nextest 設定裡設，不必每次記。
- 根治作法：找出最大的 future 用 `Box::pin` 拆開。屬於產品程式碼調整，列在 §8 給你決定。

### 2.4 每個測試的準備成本（逐項）

以 `s6_world::World::build` 為例（43 個測試檔用它）：

| 步驟 | 內容 | 成本特性 |
|---|---|---|
| `test_pool()` | 連 `postgres` 管理庫 → 查 `pg_database` → 關掉 → 再開一個 `PgPool` → `MIGRATOR.run`（31 個 migration 的 checksum 比對） | 每個測試 2 次 TCP 連線＋認證，加 migration 表查詢 |
| `reset` | 一條 `TRUNCATE` 約 50 張表 `CASCADE`，再 `UPDATE org_settings`、`reconcile_indexes` | `TRUNCATE` 每張表（含索引、TOAST）都要換新 relfilenode，要寫檔與 fsync；Docker Desktop on Windows 的磁碟 I/O 尤其慢 |
| 建使用者 | `auth::create_user` ×3（Argon2 雜湊）＋`sign_in` 1 次（Argon2 驗證） | Cargo.toml 已把 argon2／blake2 設 opt-level 3，但每次仍要配置 19 MiB 記憶體跑 4 次 |
| 其他種子資料 | 狀態、tracker、2 個專案、role、member、workflow、agent client | 二十幾條小 SQL |
| `SearchIndex::open(None)` | 記憶體內 tantivy 索引 | 便宜 |

推估每個測試準備 0.5–2 秒，佔平均 3–4.5 秒的很大一部分。要驗證只需在一個測試前後加計時；建議導入前先做（A1）。

### 2.5 G21：搜尋／通知 worker 等不到

`src/search.rs` 的一輪同步（`round_on`）只處理 `xid < H` 的 outbox 列，H 是 `pg_snapshot_xmin(pg_current_snapshot())`；`src/notifications.rs:498` 也一樣。測試輔助 `sync_all`（`search.rs:1580`）與 `s8_search_consistency.rs` 的 `catch_up` 都是「呼叫 sync → 看 frontier → sleep 20 ms → 再來」，最多 1000 次／500 次，也就是 20 秒／10 秒。

關鍵在 H 是 **cluster 層級**：PostgreSQL 的 xid 是整個 cluster 共用的，任何資料庫裡任何「已分配 xid 但還沒結束」的交易都會把 snapshot xmin 壓住。程式碼註解與 playbook 第 191 列都寫了這點（「別的 database 有已分配 xid 的未結束交易也會擋住 H」）。所以：

- 測試已經是「手動驅動 sync」了，不是等背景 worker；逾時不是因為 worker 沒排到，而是 H 被別人卡住。
- 卡住的來源可能是：同一台 PG 上的開發伺服器（`oxledger_fe_dev`）、E2E、別條工作線的 `oxledger_test_xxx`、或測試本身故意開長交易（`p2_tx_cancel`、各種 `gate.reached` 測試）。
- p1-backend-gaps.md 把這兩個逾時（`s7_redaction::search_never_shows_redacted_text`、`s8_gate::t_s2_outbox_consumed_twice`）和 Windows tantivy `PermissionDenied` 記在同一個 G21 下，其實是兩個不同原因，建議拆開記錄。

**對平行化的影響：** 換成每個測試一個資料庫後，同一個 cluster 上同時有 6–8 個測試在跑，彼此的交易都會互相壓 H。短交易只會造成毫秒級延遲；但會「故意持有交易」的測試一旦和別人同時跑，就會讓其他測試的 `sync_all` 等到逾時。這是本專案平行化最大的技術風險。

### 2.6 編譯與連結

- `tests/` 下每個 `.rs` 都是獨立的 crate、獨立連結一次整個 `oxledger` lib（含 tantivy、rmcp、axum、sqlx）。68 個 exe 共 2.26 GB 就是這個代價：改一行 `src/`，68 個 binary 全部重新連結。
- Windows 預設用 MSVC `link.exe`，連結大型 debug binary 慢；mold 不支援 Windows。
- 沒有 `.cargo/config.toml`、沒有設定 debug info 層級。

### 2.7 前端

- vitest：只有 2 個測試檔，執行時間本來就短，`--changed` 的收益很小。
- E2E 的 3–5 分鐘可能來自（未量測，依程式推估）：
  - `backend.mjs reset` 每次 `DROP/CREATE DATABASE`、跑 `oxledger seed-demo`、`docker exec psql` 套 fixture、**4 次** `oxledger user set-password`（各一個行程、各一次 Argon2）、2 次 agent-client 指令，共約 8 個子行程，還要跑全部 migration。
  - `npx vite dev` 冷啟動，第一次瀏覽每個路由時才即時編譯。
  - 18 個測試依序跑（`workers: 1`），而且彼此依賴資料順序（例如寫死 `AUTH-1`、`AUTH-3`、`AUTH-4`），不能直接開平行。
  - 每個測試都用 UI 登入（`login()` 走 `/login` 頁）。

---

## 3. 方案比較表

「預期加速」除非註明「文獻」或「實測」，都是依本專案結構推估，導入前應先量測。

| # | 方案 | 預期加速 | 導入成本 | 風險 | 適用部分 |
|---|---|---|---|---|---|
| T1 | **每個測試一個資料庫，從 template 複製**（自己寫在 `common/mod.rs`） | 解除單執行緒限制；準備時間從「TRUNCATE＋migration 檢查」變成一次 `CREATE DATABASE … TEMPLATE`（文獻：pgtestdb 約 10–20 ms、IntegreSQL 平均 11–26 ms，皆在 RAM／Linux 環境；Docker on Windows 預期較慢）。配合 8 平行，執行時間推估降到原本 1/4–1/6 | 中：改 `test_pool()`、把 14 份 `TRUNCATE` 移除或改成 no-op、管理 template 的建立與 migration 變更偵測 | **G21 加劇**（§2.5）；`max_connections`（每個 pool 預設最多 10 條，8 平行就 80＋）；template 被連線時 `CREATE DATABASE` 會失敗；測試庫要清理 | 後端整合測試 |
| T1b | **把「世界」也做進 template**（狀態、使用者含 Argon2 雜湊、專案、角色預先建好，一種 world 一個 template） | 再省下 Argon2 ×4 與二十幾條 insert；推估每個測試再省 0.2–1 s | 中：各 world 的 `build` 要拆成「建 template」與「讀回 id」兩段 | template 與 `World` 結構的同步；id 不能再寫死成新產生的 UUID，要用名稱查回 | 後端整合測試（43 個檔用 `s6_world`） |
| T2 | `#[sqlx::test]` | 同樣得到每測試獨立 DB | 中高：每個測試改屬性；pool 由 sqlx 建立，**會失去 `db::pool_options` 的 G20 release check** | sqlx 0.9 文件只寫「每個測試建新資料庫並跑 migration」，沒有寫用 template；31 個 migration 每次都跑；Kobzol（2026-06）量到每個 `#[sqlx::test]` 內嵌全部 migration 會拖慢重新編譯 | 不建議；T1 自己寫比較合適 |
| T3 | 每個測試包在交易裡最後 rollback | 理論上最快 | — | **不適用**：被測程式自己 `commit`、用多條連線、依賴 `pg_current_xact_id` 與已提交的 outbox 列；Fowler 也指出只在被測程式不 commit 時可用 | 不採用 |
| T4 | **專用測試 PostgreSQL**：另一個容器或同映像的另一個 cluster，`fsync=off`、`synchronous_commit=off`、`full_page_writes=off`，資料目錄放 tmpfs | `TRUNCATE`／`CREATE DATABASE` 的 fsync 成本幾乎歸零；同時**把開發伺服器與 E2E 的交易隔離出測試 cluster**，直接減少 G21 | 低到中：一份 docker 設定，用現有映像不需下載（若要新映像就要先問你） | 該 cluster 當機會遺失資料（測試庫可接受）；記憶體用量 | 後端整合測試、E2E 都可用 |
| T5 | **合併測試 binary**：`tests/it/main.rs` 用 `mod` 收進 70 個檔 | 文獻：Cargo 本身這樣改，測試編譯快 3 倍、磁碟產物少 5 倍（matklad 2021）；本專案預期每次改 `src/` 從連結 68 次變 1 次 | 中：每個檔的 `mod common;`、`#[path] mod world;` 改成 `crate::` 路徑；檔頭 `#![cfg(feature = "github")]` 移到 `main.rs` 的 `#[cfg] mod` 上 | 只改一個測試檔也要重編整個測試 crate（增量編譯會緩解）；名稱衝突需整理 | 後端整合測試 |
| T6 | **cargo-nextest** | 本身不減少工作量；價值在：每測試一個行程（不必再擔心 global state）、跨 binary 平行、`slow-timeout`、只對指定測試重試並標示 flaky、test group 做局部序列化、JUnit、partition。nextest 文件自己說 Windows 開行程較慢，但測試長的專案仍通常比 `cargo test` 快 | 低：一份 `.config/nextest.toml`；**需要 `cargo install` 或下載預編 binary（要你同意）** | Windows 每個測試多一次行程啟動（數十毫秒等級，對每個 1 秒以上的測試可忽略）；重試可能掩蓋真正的 bug | 後端全部測試 |
| T7 | **測試影響分析（只跑受影響的測試）** | 開發時通常只跑 1–5 個檔，從 20–30 分鐘降到 1 分鐘內 | 低：一個腳本把 `git diff --name-only` 對應到測試檔／nextest filterset；對照表手動維護 | 對照表漏寫就漏測；所以合流時仍跑全套（符合你剛定的規則） | 後端；前端用 vitest `--changed`、Playwright `--only-changed` |
| T7b | cargo-difftests（以覆蓋率自動算出受影響測試） | 自動化程度最高 | 高：要 nightly Rust 與 LLVM 覆蓋率插樁 | 插樁讓測試變慢；工具小眾、GitHub 上沒有正式 release | 不建議現階段採用 |
| T8 | **把非同步等待改成確定性的** | 消除 G21 類逾時；也省下固定 sleep 約 15 s | 中：見 §4.4 | 改測試輔助程式；`tokio::time::pause` 和真實 DB I/O 混用時，自動前進的時鐘可能讓逾時提早觸發 | 後端整合測試 |
| T9 | 連結器改 `rust-lld`（MSVC 上的 `lld-link`） | Linux 上 Rust 1.90 預設改用 lld，ripgrep 增量重建連結時間少 7 倍、整體少 40%（官方部落格）；Windows 上使用者回報不一致，Rust 官方追蹤 issue 列了 MSVC 上連 libtest 的 undefined symbol 問題 | 低：`.cargo/config.toml` 一行；`rust-lld` 隨 rustup 工具鏈提供，可能需要 `llvm-tools` 元件（新增元件要先問你） | 可能連結失敗；收益要實測 | 編譯 |
| T10 | 減少 debug info（`[profile.dev.package."*"] debug = false` 或 `debug = "line-tables-only"`） | 連結輸入變小、exe 變小（現在 68 個共 2.26 GB）；Kobzol 指出 debug info 是重建時間的主要成本之一 | 低：Cargo.toml 幾行 | 依賴 crate 的 backtrace 少了行號 | 編譯 |
| T11 | sccache | 對**本機增量開發幾乎沒幫助**：要關掉增量編譯，而且有連結步驟的 crate（bin、測試 binary）不能快取 | 中 | — | 只對未來的 CI 冷建置有用；現在不建議 |
| T12 | 測試分層（金字塔）：純邏輯往 `src/` 單元測試移，整合測試只驗證跨 DB 的契約，E2E 只留主要使用者流程 | 長期最大；單元測試毫秒級 | 高：逐步重構 | 移動時可能漏掉現在只靠整合測試覆蓋的行為 | 全部 |
| F1 | vitest `--changed`／`vitest related <檔案>` | 2 個測試檔，收益小 | 零：現有功能 | — | 前端單元測試 |
| F2 | E2E 以 template 取代每次 seed（`CREATE DATABASE oxledger_fe_e2e TEMPLATE oxledger_fe_e2e_tpl`） | 省掉 ~8 個子行程、全部 migration 與 4 次 Argon2；推估 reset 從數十秒降到 1–2 秒 | 低：`backend.mjs` 改一段；template 在 migration 或 fixture 變動時重建 | template 過期 | E2E |
| F3 | E2E 用 `vite build` + `vite preview` 取代 `vite dev` | 少了每個路由第一次的即時編譯 | 低 | 和開發時的行為略有差異（但更接近正式環境） | E2E |
| F4 | Playwright `storageState` 重用登入、以 API 建資料取代 UI 操作 | 每個測試省幾秒 | 低到中 | 「登入流程」本身仍要留一個 UI 測試 | E2E |
| F5 | Playwright 平行 worker，每個 worker 一個資料庫＋一個後端（埠號 = 基底＋`parallelIndex`） | 18 個測試用 3–4 個 worker，推估 3–5 分鐘降到 1–2 分鐘 | 中高：測試目前依賴執行順序（寫死 `AUTH-1`…），要先讓每個測試自己建資料 | 同 cluster 的 G21 問題；記憶體 | E2E |
| F6 | Playwright `--only-changed`（1.46 起） | 開發時只跑改到的 spec；但現在只有 1 個 spec 檔，**效果等於沒有**，除非先拆檔 | 零 | — | E2E |

---

## 4. 各方案細節

### 4.1 Template database（T1、T1b、F2）

作法（PostgreSQL 原生，不需新工具）：

1. 每次測試行程開始，用 advisory lock 保護，檢查名為 `oxledger_tpl_<migration 雜湊>` 的資料庫是否存在；不存在就建立、跑 migration、（T1b）灌入 world 的基礎資料，然後 `ALTER DATABASE … IS_TEMPLATE true`，並確保**沒有任何連線留在 template 上**。
2. 每個測試 `CREATE DATABASE oxledger_t_<uuid> TEMPLATE oxledger_tpl_<雜湊>`，連上去就能用，不需要再跑 migration（`db::connect` 的 `MIGRATOR.run` 會發現都已套用，只剩一次查詢）。
3. 測試通過就 `DROP DATABASE … WITH (FORCE)`；失敗就保留方便除錯（pgtestdb 與 sqlx::test 都是這個慣例），下次啟動清掉舊的。

注意事項：

- PostgreSQL 文件：template 有其他連線時 `CREATE DATABASE` 會失敗；預設 `STRATEGY WAL_LOG` 對小型 template 最有效率，`FILE_COPY` 會在前後各強制一次 checkpoint，平行建立時反而拖累整台伺服器，所以用預設即可。
- migration 雜湊要包含 `migrations/` 內容，改了 migration 就自動換新 template（pgtestdb 用 migrator 的 `Hash()` 做同一件事）。
- 連線數：每個測試的 pool 應限制在 2–4 條；PostgreSQL `max_connections` 預設 100。
- 搜尋索引：測試用 `SearchIndex::open(None)` 是記憶體索引，不受影響；用實體目錄的少數測試已各自用暫存目錄。
- 不需要 IntegreSQL（Go 寫的獨立服務、要 docker pull）或 sqlx-pg-test-template（0.1.x、維護者未確認）；邏輯不到 100 行，自己寫在 `common/mod.rs` 比較可控。

### 4.2 cargo-nextest（T6）

- 每個測試一個行程；`[test-groups]` 可定義「最多 N 個同時跑」的群組，`max-threads = 1` 就是序列化。適合把會故意持有長交易的測試（`p2_tx_cancel`、`gate.reached` 那類、`m1_8_notifications` 的 120 秒迴圈）放進同一個序列群組，避免它們壓住其他測試的 H。
- `retries` 可以只對特定測試（filterset）開，通過的會被標為 flaky；`flaky-result = "fail"` 可讓合流時 flaky 也算失敗，避免掩蓋問題。
- filterset 有 `test()`、`binary()`、`package()`、`rdeps()` 等；本專案只有兩個 crate，`rdeps(ox-core)` 只能分出「改 ox-core 要跑全部」這種粗粒度，檔案層級要靠 T7 的對照表。
- `--partition slice:m/n` 主要是給多台 CI 機器，本機用不到。
- setup scripts 仍是實驗功能（`experimental = ["setup-scripts"]`），可用來在測試前建 template，但可能變動；建議 template 建立仍寫在 `common/mod.rs` 裡用 advisory lock，不依賴實驗功能。
- `RUST_MIN_STACK` 在 nextest 下照樣有效（測試仍在 libtest 開的執行緒上跑）。

### 4.3 合併測試 binary（T5）

matklad（rust-analyzer 作者）的建議：大型專案的整合測試放成一個 crate，`tests/it/main.rs` 引入其他模組。Cargo 自己這樣改，測試編譯快 3 倍、磁碟產物少 5 倍；另一個例子的執行時間從 20 秒降到 13 秒。本專案 68 個 exe 共 2.26 GB，預期磁碟與連結時間都有同量級的改善。

合併後測試名稱會變成 `s6_read::xxx`，T7 的對照表與 nextest filterset 就用 `test(s6_read::)` 選，不再用 `binary()`。

### 4.4 把非同步等待改成確定性的（T8，處理 G21）

現在的測試已經是「自己呼叫 `sync`」，不必再等背景 worker；問題在 H。由便宜到徹底：

1. **失敗時印出卡住的是誰**（先做，成本最低）：`sync_all` 逾時前查 `pg_stat_activity` 裡 `backend_xid`／`backend_xmin` 最老的連線（資料庫、`application_name`、開始時間、查詢），放進錯誤訊息。下次偶發就知道是開發伺服器、E2E、還是同套測試裡的誰。
2. **測試 cluster 與開發／E2E 分開**（T4）：直接去掉最常見的外部來源。
3. **同套測試內的長交易**：用 nextest test group 序列化，或讓那些測試在結束前一定釋放交易。
4. **固定 sleep 改成可注入的時間**：已有 `keep_events`、`limits.*` 這類可設定的時長，延伸到其他寫死 1–3 秒的地方；`tokio::time::pause` 只適合不碰真實 DB 的測試（需要 `current_thread` runtime 與 `test-util` feature，時間暫停時 runtime 一閒下來就自動跳到下一個計時器，等 DB I/O 時可能讓逾時提前觸發）。
5. Fowler 的原則：不要用裸 sleep；callback 最好，polling 次之但要有清楚的逾時錯誤。現有程式已是 polling＋明確錯誤，缺的是 1 的診斷資訊。

不建議：為了測試讓 H 改看單一資料庫的 xid。xid 本來就是 cluster 層級的，這樣做會讓測試驗證的語意和正式環境不同。

### 4.5 測試影響分析（T7）

業界大公司（Google TAP、Meta 的 predictive test selection）用的是建置圖或機器學習，對單一 crate 的專案過重。本專案可行的最小版本：

- 一個對照表：`src/search.rs` → `s8_*`、`p2_search`、`m1_7_search`；`src/notifications.rs` → `m1_8_notifications`；`src/auth*` → `s0_auth`、`s1_identity`…；改到 `migrations/`、`common/`、`s6_world/` → 全跑。
- 腳本讀 `git diff --name-only main...HEAD`，輸出 nextest filterset 或 `cargo test --test …` 參數。
- 合流時一律全套（你剛定的規則），用來抓對照表漏掉的部分。

### 4.6 測試分層（T12）

Fowler／Vocke 的測試金字塔與 Google Testing Blog 的建議一致：大量快的單元測試、較少的整合測試、極少的 E2E。本專案 402 個整合測試對 116 個單元測試，形狀是倒過來的。很多整合測試（例如權限矩陣、命令驗證、JSON 形狀）在純函式層就能驗證。這是長期工作，不建議一次大搬，而是「新寫的邏輯優先寫單元測試」。

### 4.7 前端（F1–F6）

- vitest `--changed [since]` 只跑受變更影響的測試；`vitest related <檔案>` 依靜態 import 找出涵蓋這些檔案的測試。
- Playwright：worker 是獨立行程，各自有瀏覽器；`fullyParallel: true` 讓同檔測試也平行；`testInfo.parallelIndex`（0 到 workers-1）適合拿來決定每個 worker 的資料庫名稱與後端埠號；`--shard` 是給多台機器的。官方建議用 `testInfo.testId` 產生唯一資料、測試之間不要互相依賴。
- 現在的 E2E 測試彼此依賴順序，要平行必須先改成每個測試自己建資料（F4 的 API 建資料）。投入產出比最好的是 F2（template）與 F3（preview），不必改測試本身。

---

## 5. 建議的導入順序

### 第 0 步（半天內，不用新工具）：量測與診斷

- 在一個代表性測試前後加計時，拆出 `test_pool`／`reset`／Argon2／測試本體的時間（需要你同意跑一個測試檔，A1）。
- `sync_all`／`catch_up` 逾時時印出最老交易（§4.4-1）。
- `RUST_MIN_STACK` 寫進 `.cargo/config.toml` 的 `[env]`。

### 第 1 件：每個測試一個資料庫（template）＋專用測試 cluster＋平行（T1、T1b、T4）

- 預期效果：解除 `--test-threads=1`。準備時間從推估的 0.5–2 秒降到 0.05–0.3 秒（Docker on Windows 的 `CREATE DATABASE` 要實測）；用 6–8 平行，**全套推估 20–30 分鐘 → 4–8 分鐘**。測試 cluster 和開發／E2E 分開後，外部來源造成的 G21 應該消失。
- 不需安裝新工具，`cargo test` 預設的多執行緒就能先吃到大部分效果。

### 第 2 件：合併成一個測試 binary＋減少依賴的 debug info（T5、T10）

- 預期效果：改 `src/` 後的重新編譯與連結從 68 次變 1 次。文獻值是測試編譯快 3 倍、產物少 5 倍；本專案 68 個 exe 2.26 GB，預期降到數百 MB。這主要改善「改一行就要等很久才開始跑」的開發體驗，也是第 3 件「只跑受影響的測試」能真正快起來的前提（不然選了 1 個測試仍要等連結）。

### 第 3 件：cargo-nextest＋依 git diff 選測試（T6、T7）

- 預期效果：開發時只跑受影響的檔，通常 1 分鐘內；合流時全套平行，加上 test group 讓長交易測試序列化，G21 類失敗可被隔離與標示；`slow-timeout` 讓卡住的測試直接顯示，不必等 20 分鐘後才發現。全套再比第 1 件多快一些（跨 binary 平行；合併 binary 後差距變小）。
- 需要你同意安裝 cargo-nextest（A3）。

之後再做：前端 F2＋F3（低成本）、T8 剩下的固定 sleep、T9 lld 實測、T12 長期分層。

---

## 6. 工具維護者與所在地

| 工具 | 維護者 | 所在地 | 狀態 |
|---|---|---|---|
| PostgreSQL template／非持久化設定 | PostgreSQL Global Development Group | 國際社群 | 已在使用 |
| sqlx（含 `#[sqlx::test]`） | LaunchBadge | 美國 | 已在使用 |
| cargo-nextest | nextest-rs，主要作者 Rain（sunshowers） | 美國 | 未安裝，需同意 |
| pgtestdb（只作參考，Go） | Peter Downs | 美國 | 不採用 |
| IntegreSQL（只作參考，Go） | all about apps GmbH，現任維護者 Mario Ranftl | 奧地利（維也納） | 不採用 |
| sqlx-pg-test-template | 未確認 | 未確認 | 不採用（改自己寫） |
| cargo-difftests | dnbln | 未驗證 | 不採用（要 nightly） |
| sccache | Mozilla | 美國 | 不採用（現階段無益） |
| rust-lld／lld-link | Rust 專案／LLVM 專案 | 國際社群 | 隨工具鏈，可能需 `llvm-tools` 元件 |
| tokio（`time::pause`） | tokio-rs | 國際社群（主要在美國） | 已在使用 |
| Playwright | Microsoft | 美國 | 已在使用 |
| Vitest／Vite | Vitest 團隊，由 VoidZero 公司支持；VoidZero 創辦人為 Evan You（尤雨溪） | 團隊成員分散多國 | **已在使用**；本研究只建議用既有功能、不新增依賴。是否符合「不用中國產品」的規則請你判斷（A6） |

---

## 7. 研究過程

### 7.1 讀了哪些程式

- `tests/common/mod.rs`（`test_pool`）、`tests/s6_world/mod.rs`（`reset`、`World::build`）、`s8_search_consistency.rs`（`catch_up`）、`m1_11_gate.rs`（`gate_world` 建在 `World::build` 上）、`s8_gate.rs`、`s7_redaction.rs` 的 G21 測試。
- `src/search.rs`（`sync_all`、`sync`、`round_on` 的 H 計算）、`src/notifications.rs:498`、`src/db.rs`（`connect` 每次跑 `MIGRATOR.run`、G20 的 release check）。
- `Cargo.toml`（argon2 opt-level 3）、`crates/oxledger/Cargo.toml`（`test-support` feature）。
- `web/playwright.config.ts`、`web/e2e/backend.mjs`、`web/e2e/flows.spec.ts`、`web/vite.config.ts`、`web/package.json`。
- `docs/playbook.md`（第 35–37、138、191–192 列）、`docs/impl/p1-backend-gaps.md`（G18、G21）、`docs/impl/m1-11-e5.md`、commit ccc5a6d。

### 7.2 執行過的唯讀指令

- `ls`、`wc -l`、`grep -c` 計算檔案、測試數、`TRUNCATE` 與 sleep 位置。
- `target/debug/deps/<名稱>.exe --list`（`s6_read`、`m1_11_gate`、`s8_search_consistency`），只列出不執行。
- `ls -la` 統計 68 個測試 exe 的大小。
- `git log -S RUST_MIN_STACK`、`git show` 找堆疊設定的來源。

### 7.3 網路查詢

| 查詢／網址 | 得到什麼 |
|---|---|
| docs.rs `sqlx::test` | 每個測試建新 DB、跑 migration、通過就刪、失敗保留；需要 superuser；共用連線上限。文件沒有提 template |
| 搜尋「sqlx::test postgres template database migrations each test slow」 | sqlx-pg-test-template；Kobzol 2026-06 的 sqlx 測試重建時間文章 |
| kobzol.github.io（optimizing sqlx test rebuild time） | ~350 個 `#[sqlx::test]`＋30 個 migration：no-op 重建 7.5 s，內嵌 migration 佔約 2.5 s；共用 `static MIGRATOR` 可降到 5 s；連結 220 MiB 測試 binary lld 約 1 s |
| github.com/peterldowns/pgtestdb | template＋migration 雜湊；複製約 10 ms、每測試約 20 ms；建議 tmpfs、`fsync=off` |
| github.com/allaboutapps/integresql | 預先建好的 DB 池；50 測試平均 11 ms、280 測試平均 26 ms |
| postgresql.org/docs/18 CREATE DATABASE | `WAL_LOG` 適合小 template；`FILE_COPY` 前後強制 checkpoint；template 有連線會失敗 |
| postgresql.org/docs/18 non-durability | `fsync`、`synchronous_commit`、`full_page_writes` 關閉、RAM disk、加大 checkpoint 間隔 |
| nexte.st why-process-per-test | 行程隔離的理由；Windows 開行程較慢，但測試長時通常仍比 `cargo test` 快 |
| nexte.st test-groups、partitioning、retries、filterset reference | `max-threads`、`slice:m/n`、只對指定測試重試與 flaky 標示、`rdeps()` 等 |
| 搜尋「nextest setup scripts」 | setup scripts 仍是實驗功能 |
| 搜尋「cargo-difftests」 | 需要 nightly 與 LLVM 覆蓋率；沒有正式 release |
| matklad.github.io（delete cargo integration tests） | 合併成單一測試 crate；Cargo 測試編譯 3 倍快、產物 5 倍小 |
| blog.rust-lang.org（rust-lld on 1.90.0 stable） | Linux x86_64 預設 lld；ripgrep 增量連結 7 倍快、整體 40% |
| 搜尋「rust windows msvc lld-link」 | Windows 上效果不一；官方追蹤 issue 列有 MSVC 連 libtest 的問題 |
| github.com/mozilla/sccache docs/Rust.md | 不能搭配增量編譯；有連結步驟的 crate 不能快取 |
| docs.rs tokio `time::pause` | 需 `current_thread`、`test-util`；閒置時自動跳到下一個計時器 |
| martinfowler.com nonDeterminism | 不用裸 sleep；callback 優先、polling 次之；交易 rollback 只在被測程式不 commit 時可用 |
| vitest.dev guide/cli | `--changed`、`vitest related`、`--shard` |
| playwright.dev test-parallel | worker、`fullyParallel`、`parallelIndex`、每個 worker 獨立資料、`--shard` |
| 搜尋「playwright --only-changed 1.46」 | 1.46 新增；只找到二手來源，沒找到官方 release notes 頁 |

另外引用但沒有在本次重新抓取的：Fowler／Vocke〈The Practical Test Pyramid〉（martinfowler.com/articles/practical-test-pyramid.html）、Google Testing Blog〈Just Say No to More End-to-End Tests〉（2015）、Playwright 的 auth／API testing 文件、Cargo profiles 文件（`debug` 設定）。這幾項是方向性引用，沒有用到其中的數字。

### 7.4 踩到的坑與限制

- **G21 這個代號在 repo 裡有兩個意思**：`decisions.tsv` 與 `s6b-checklist.md` 的 T-G21 是 GitHub 補送測試；`p1-backend-gaps.md` 的 G21 是 Windows tantivy `PermissionDenied`，附帶記了兩個索引 20 秒逾時。本報告的「G21」指後者裡的逾時。建議把逾時拆成獨立項目。
- **任務說「500 多個測試」**：實際是整合測試 402＋`src/` 單元測試 116＝518，兩者合計才符合。
- **不能跑測試，所以沒有每個測試準備時間的實測**；所有加速倍數都是推估或文獻值，文獻多在 Linux／RAM disk 上量的，Docker Desktop on Windows 可能差很多。
- sqlx::test 文件沒有寫內部是否用 template，不能當成它很快；同時它會繞過專案自己的 `db::pool_options`（G20 修補），這是本專案特有的不採用理由。
- 第一次在 `target/` 上跑 `du -sh` 超過 2 分鐘被移到背景，改用 `ls -la` 加總 exe 大小。
- `RUST_MIN_STACK` 的原因在 repo 裡找不到文字記錄，§2.3 是推測。
- Playwright `--only-changed` 只找到二手來源，使用前應以 playwright.dev 為準。
- `rust-lld` 在 Windows MSVC 的效果證據互相矛盾，必須自己量。

---

## 8. 需要你決定的問題（A 類）

**A1. 能不能為了量測跑一個測試檔？**
- 選項：(a) 用自己的資料庫 `oxledger_test_bench` 跑 `s6_read`（10 個測試）並加臨時計時；(b) 不量測，直接照推估做。
- 建議：(a)。
- 代價：(a) 要改測試輔助程式加計時（量完可還原）、約 5 分鐘、會在 PG 上建一個資料庫；(b) 可能優化錯地方。

**A2. 測試要不要用另一個 PostgreSQL（專用測試 cluster）？**
- 選項：(a) 另開一個容器（用現有 PostgreSQL 18 映像，不下載），`fsync=off` 等設定、資料放 tmpfs、另一個埠號，只給測試用；(b) 同一個容器內多開一個 cluster；(c) 繼續共用 `oxledger-pg`，只改成每測試一個資料庫。
- 建議：(a)。
- 代價：(a) 多吃約 0.5–1 GB 記憶體、多一份 docker 設定、要寫進 playbook；(c) 零設定，但平行後 G21 會更常出現，因為開發伺服器與 E2E 的交易仍會壓住 H。若現有映像不在本機，(a) 就需要下載，要先再問你。

**A3. 要不要安裝 cargo-nextest？**
- 選項：(a) 安裝（`cargo install cargo-nextest --locked` 或官方預編 binary），維護者在美國；(b) 不裝，只用 `cargo test` 多執行緒＋自己寫的序列鎖。
- 建議：(a)，但排在第 1、2 件之後。
- 代價：(a) 一次下載與編譯（數分鐘），之後要記得用 `cargo nextest run`；Windows 每個測試多一次行程啟動。(b) 沒有 per-test 逾時、重試、test group，長交易測試要自己用 mutex 序列化。

**A4. 測試 binary 要不要合併成一個？**
- 選項：(a) 全部合併成 `tests/it/`；(b) 依領域合成 5–8 個（auth、commands、search、ai、github…）；(c) 維持現狀。
- 建議：(a)。
- 代價：(a) 一次性搬動 70 個檔的 `mod` 宣告，git 歷史會顯示整批移動（可用 `git mv` 保留追蹤）；改任何測試檔都重編整個測試 crate。(b) 折衷，連結次數 5–8 次。(c) 每次改 `src/` 連結 68 次。

**A5. 重試 flaky 測試的政策？**
- 選項：(a) 只在開發時對已知 flaky 名單重試 1 次，合流時 `flaky-result = "fail"`；(b) 開發與合流都允許重試；(c) 完全不重試。
- 建議：(a)。
- 代價：(a) 合流仍可能因 flaky 卡住，但問題不會被藏起來；(b) 最順，但 G21 這類真實的時序問題會被掩蓋；(c) 開發時偶發失敗會打斷工作。

**A6. Vitest／Vite 是否符合「不用中國產品」？**
- 背景：專案已在用；VoidZero 創辦人 Evan You（尤雨溪）。本研究只建議用它既有的 `--changed`，不新增依賴。
- 選項：(a) 視為可用，維持現狀；(b) 列入日後替換評估。
- 建議：由你判斷，這是規則解讀問題，不是技術問題。
- 代價：(b) 替換 Vite 牽動整個 SvelteKit 建置鏈，成本很高。

**A7. `RUST_MIN_STACK` 要怎麼處理？**
- 選項：(a) 寫進 `.cargo/config.toml` 的 `[env]`；(b) 找出大 future 改用 `Box::pin`，讓預設堆疊就夠；(c) 維持手動設定。
- 建議：先 (a)，之後有空再 (b)。
- 代價：(a) 一個新設定檔，也會影響 `cargo run`（通常無害）；(b) 要改產品程式碼並重新驗證；(c) 忘了設就 stack overflow。

**A8. E2E 要不要改成可平行？**
- 選項：(a) 只做 template＋`vite preview`（F2、F3），測試不動；(b) 再加 storageState 與 API 建資料（F4）；(c) 全面改成每個 worker 獨立資料庫與後端（F5）。
- 建議：先 (a)，看結果再決定 (b)。
- 代價：(a) 改 `backend.mjs` 與設定，預期省 1–2 分鐘；(c) 要重寫 18 個測試的資料準備，並承擔同 cluster 的 G21 問題。
