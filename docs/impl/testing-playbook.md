# 後端測試 playbook

後端整合測試怎麼跑、為什麼這樣跑（T1：D1(a)(d)(e)、D2、D4、D7、D10、D13；決定見 `decisions.tsv`，依據見 `docs/research/08-review-05-06-07.md` §6）。

## 一次性準備：測試專用 PostgreSQL

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/test-pg.ps1            # 沒有就建立，停了就啟動
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/test-pg.ps1 -Recreate  # 砍掉重建（資料全清）
```

- 容器 `oxledger-pg-test`，`127.0.0.1:55433`，帳密 `oxledger`／`oxledger_dev`。只給測試用；開發用的 `oxledger-pg`（55432）不動。
- `-Name`／`-Port` 可以另開一個容器（預設 `oxledger-pg-test`／55433）。名稱一定要以 `oxledger-pg-test` 開頭，否則腳本直接 throw；`-Recreate` 只刪 `-Name` 指定的那一個。
- 用本機的 `postgres:18` 映像，`--pull never`；本機沒有映像時腳本會停下來，不會下載。
- 不持久：`fsync=off`、`synchronous_commit=off`、`full_page_writes=off`，資料目錄是 tmpfs（4 GB 上限）。容器重啟就是空的 cluster，`test_pool()` 會自動重建需要的 DB。
- `max_connections=300`：一套全套平行（12 slot）用不到一半，見下面「連線數」。

## 跑法

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
cargo nextest run -p oxledger                                  # 全套，test-support
cargo nextest run -p oxledger --features test-support,github  # 加 GitHub 測試（s6b_github_sync）
cargo nextest run -p oxledger --features test-support,ai      # 加 AI 測試（m1_10_ai_run、m1_11_gate_ai、m1_11_e5）
cargo nextest run -p oxledger -E 'binary(s6_read)'            # 只跑一個檔
```

- 不需要設 `RUST_MIN_STACK`，也不需要 `--test-threads=1`（D13，見下）。
- 設定在 `.config/nextest.toml`：不自動重試（D7）、60 秒標示慢測試但不砍、`fail-fast = false`。
- 平行數預設是邏輯核心數（這台 12）。
- 磁碟：每換一組 features（test-support、ai、github、clippy 各組）就多一整套測試 binary。T1 期間一個 worktree 的 `target/debug` 長到 189 GB 把 F: 塞滿，連結失敗（`no space on device`、link.exe 1318）。滿了就 `cargo clean -p oxledger` 並刪 `target/debug/incremental`（T1 這次釋放 177 GB）。

### 驗證子代理、其他工作線：另開一個容器

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/test-pg.ps1 -Name oxledger-pg-test-verify -Port 55434
$env:OXLEDGER_TEST_DATABASE_URL = 'postgres://oxledger:oxledger_dev@127.0.0.1:55434/oxledger_verify'
cargo nextest run -p oxledger
docker rm -f oxledger-pg-test-verify   # 用完就刪（資料本來就不持久）
```

**兩套測試不要同時跑在同一個容器上**，換前綴也不行。前綴只讓兩邊的 DB 名稱不同（不會互相 TRUNCATE 或 DROP），
但搜尋與通知等的 snapshot horizon H 是整個 cluster 共用的：另一套測試的寫入交易、`DROP DATABASE` 都會壓住這一套的 H。
nextest 的 `threads-required`、`drop-database` 群組只管得到同一次 run 裡的測試，管不到另一個行程，所以「單獨跑」的測試會失敗或卡住。
主流程用 `oxledger-pg-test`（55433），驗證子代理用 `oxledger-pg-test-verify`（55434），其他工作線再往後開（名稱以 `oxledger-pg-test` 開頭、埠不重複）。
同一個容器上同時只能有一套測試在跑。

## DB 怎麼選（`tests/common/mod.rs`）

| 情況 | 用的 DB |
|---|---|
| nextest（預設） | `127.0.0.1:55433` 上的 `<前綴>_<NEXTEST_TEST_GLOBAL_SLOT>`，例如 `oxledger_test_0` … `oxledger_test_11` |
| `cargo test`（沒有 slot） | `<前綴>`，例如 `oxledger_test`；這時要 `--test-threads=1`，因為同一個 binary 內的測試共用這個 DB |
| 有設 `OXLEDGER_TEST_DATABASE_URL` | 改用那台 server 與那個 DB 名稱當前綴；在 nextest 下一樣會加 `_<slot>`。URL 的 `?query` 會帶到每一條連線（slot DB、`postgres` 管理連線、暫時 DB），測試一律用 `TestDb::url()`／`admin_url()`／`url_for(name)` 組 URL，不要自己拼 `base` |
| 暫時 DB（升級測試、`migration_0007_repairs_dates`、`s5_migrations_from_empty`） | `<slot DB>_<後綴>`，例如 `oxledger_test_3_upg14`；跟著前綴走 |

- slot 是 nextest 保證「同一時間只有一個測試在用」的號碼，所以每個測試在自己的 slot DB 上照舊用 TRUNCATE reset（D2(a)），整套跑下來不會為了平行而 DROP DATABASE。
- slot DB 不存在時 `test_pool()` 會自動建立；跑完不刪，下次沿用。
- 前綴與 slot 名稱只允許英數字與底線，長度不超過 63。

## 會 DROP DATABASE 的測試：序列群組（D1(e)）

13 個測試檔、16 個測試會建再刪暫時 DB：`m1_0/2/3/4/5/6/7/8/9/10_upgrade`、`m2_0_upgrade` 全部，以及 `s3_permissions::migration_0007_repairs_dates`、`s5_changesets::s5_migrations_from_empty`。

- `DROP DATABASE` 會持有 xid 一直到 checkpoint 做完，期間整個 cluster 的 snapshot horizon 被壓住，所有 `sync_all`／`dispatch_all` 都在等（G21，08 §1）。
- 這些測試放在 nextest 的 `drop-database` 群組（`max-threads = 1`），一次只跑一個；測試容器 `fsync=off`＋tmpfs，checkpoint 很短。
- **群組只讓 DROP 測試彼此依序跑，沒有避開 `sync_all`／`dispatch_all`**：DROP 測試跑的時候，旁邊的測試照樣在跑，可能被壓住幾百毫秒。這是刻意的取捨：
  `fsync=off` 讓 checkpoint 很短，16 個測試合計約 13 秒（T1 第三輪實測：16 個全過，各測試時間合計 13.5 秒），`sync_all` 有 20 秒可以等；
  把它們設成 `threads-required = num-test-threads` 會讓全套多出這 13 秒的獨佔時間，還要等前面的測試都結束才能開始。
- 新增會 DROP DATABASE 的測試時，要把它加進 `.config/nextest.toml` 的 filter（`cargo nextest show-config test-groups` 可以看目前的成員）。
  `tests/t1_test_infra.rs` 的 `nextest_groups_cover_the_tests_they_must`（G4）會檢查：測試檔裡直接或經過同檔 helper 執行 `DROP DATABASE`／`WITH (FORCE)` 的測試都要在群組裡，
  而且 nextest.toml 用 `test(=…)` 點名的測試都真的存在。忘了加或改了名，這個測試就會失敗。

## 必須單獨跑的測試（`threads-required = num-test-threads`）

搜尋與通知只處理 xid 低於 cluster 共用 horizon H 的 outbox 列。同一個 cluster 上任何一個還沒結束、已分配 xid 的交易都會壓住 H，所以平行時：

- **對 H 敏感的測試**：只呼叫一次 `sync`／`sync_round`／`dispatch_round`，或停在 fault gate 等 worker 走到某一步，就斷言自己的列已經處理。別的測試剛好有寫入交易時，這一輪看不到自己的列，測試失敗或永遠等不到 gate（T1 第一次平行跑：p2_search 3 個、s8_gate 1 個、m1_8 2 個失敗，s8_gate_probes 卡 6 分鐘）。
  名單：`p2_search`、`s8_gate_probes`、`s8_search_consistency` 全檔；`s7_redaction::search_never_shows_redacted_text`、`s8_gate::t_s2_outbox_consumed_twice`、`m1_7_search::legacy_generation_on_disk_is_opened`、`m1_8_notifications` 的 7 個（見 nextest.toml；`t_m1_71_private_note`、`t_m1_73_private_issue` 是 P3 缺口第二輪的獨立驗證在大量平行時看到失敗後加的）。
- **長時間持有 xid 的測試**：`p2_strategy` 的 2 個、`m1_2_views` 的 2 個一次交易寫上萬筆 issue，平行時要 20–40 秒，會讓旁邊所有 `sync_all` 等滿 20 秒逾時（s1_identity 就這樣失敗過，`describe_xid_holders` 直接指出是 `UPDATE projects SET issue_counter = issue_counter + 12000`）。
- **`t1_test_infra::horizon_timeout_message_names_the_holder`**（G2）：在 cluster 最老的 5 個 xid 持有者裡找自己的交易，平行時會被別的測試的交易擠掉。

這些測試用 `threads-required = 'num-test-threads'`：等其他測試都結束才開始，跑的時候沒有別人。
用 `cargo nextest list -E <兩段 filter>` 核對（P3 缺口第二輪）：共 46 個，45 個會執行，另外 1 個是 `#[ignore]` 的 `m1_8_notifications::perf_dispatch_p95`（平常不跑）。
其中 T1 原本的 42 個可執行，加上第三輪新增的 G2 測試，再加上 P3 缺口第二輪的 `m1_8_notifications` 2 個。合計約 100 秒（單獨跑時每個 1–2 秒，p2_strategy 各約 20 秒）。
新寫的測試如果直接呼叫 `sync`／`sync_round`／`dispatch_round` 或 fault gate，要用 `_all` 版本，或加進這份名單。

暫時保留單獨跑的原因：這些測試斷言的是「worker 走一輪就處理到」或「gate 一定等得到」，正確性建立在「cluster 上沒有別人的寫入交易」這個前提。
平行時這個前提不成立，失敗的是測試的假設，不是產品（H 被別的交易壓住時延後處理是 03 §4.7 的設計）。改寫要逐一確認每個斷言的意圖，T1 範圍內先隔離，不改斷言。

### 待辦：改寫成可以平行的測試

- [ ] `p2_search`、`s8_search_consistency`、`s7_redaction::search_never_shows_redacted_text`、`m1_7_search::legacy_generation_on_disk_is_opened`：改成迴圈等明確條件（`frontier() > X`、自己的列已處理），像 `s8_search_consistency` 的 `catch_up`，或改用 `sync_all`。
- [ ] `m1_8_notifications` 的 7 個：改用 `dispatch_all` 或等自己的 outbox 列被消耗；`perf_dispatch_p95` 要另外想量測方式（平行時的 p95 本來就不準）。
- [ ] `s8_gate::t_s2_outbox_consumed_twice`、`s8_gate_probes`：gate 等待加上「H 已越過自己的列」的前置條件，不要假設下一輪一定處理到。
  T1b 已改 `s8_gate_probes::no_waiter_passes_before_publish`（`common::wait_horizon_past`）與 `p2_search::no_token_is_satisfied_before_a_generation_serves`（輪詢到 F 越過）；兩檔仍在單獨跑名單，要平行跑全套驗過才拿掉。
- [ ] `p2_strategy` 的 2 個、`m1_2_views` 的 2 個：把一次寫上萬筆的交易拆小，或改用 `COPY`／分批 commit，讓 xid 不要一次持有 20–40 秒。
- 完成一項就把它從 `.config/nextest.toml` 的 override 拿掉，平行跑全套三次都過才算。

## 可觀測性（T2）

- `OXLEDGER_LOG_FORMAT=json` 讓 `oxledger serve`／`stdio` 的 log 改成一行一個 JSON 物件（預設 `text`）；本機要用 `jq` 過濾時再開。規格見設計 02 §4.3、部署說明見 `upgrade-notes.md`「T2 可觀測性」。
- `tests/t2_observability.rs` 的指標斷言（`HTTP.count(...)`、`POOL_ACQUIRE_TIMEOUTS`、worker 計數）讀的是**程序內**的計數器。nextest 每個測試一個程序，所以計數只來自那個測試；
  用 `cargo test` 跑時同一個 binary 的測試共用計數器，`route_labels_are_templates_without_values`、`pool_acquire_timeouts_are_counted` 的精確數字會對不上。請用 nextest，或 `cargo test --test t2_observability <測試名>` 一次跑一個。
- `readyz_reports_database_migrations_and_workers` 的「migration 落後」用一個不存在的版本號模擬（`api::readyz_routes` 吃預期的版本清單），不動 slot DB 的 `_sqlx_migrations`，所以不需要暫時 DB，也不在 `drop-database` 群組。
- `s8_gate_extra::hidden_content_stays_out_of_logs_and_audit` 也走 HTTP 與 MCP（`http_probe`），文字與 JSON 兩種格式各跑一次。新增 log 欄位時要讓它繼續涵蓋。它用的是產品的 `telemetry::subscriber`（`RUST_LOG=trace`、rmcp 上限開著）；不要在測試裡自己組 subscriber 或加 `rmcp=info` 繞過，`t2_observability::rmcp_debug_logs_are_capped_unless_allowed` 才會一起守住 rmcp 的上限。
- 需要「沒有 oxledger 資料表的資料庫」的測試（500、migration 讀不到）連測試伺服器的 `postgres` 資料庫，用 `t2_observability` 的 `tiny_admin_pool`：它**不跑 migration**。不要對 `admin_url()` 呼叫 `db::connect_with`，那會把 migration 套進 `postgres`。
- `readyz_gives_up_after_two_seconds` 在本機開一個只 accept、不回話的 TCP 埠當資料庫，量 `/readyz` 在 2 秒左右回 503（上限 5 秒，`/readyz` 池自己的 acquire timeout 是 10 秒）。

## 逾時時印出是誰壓住 horizon（D1(d)）

`SearchIndex::sync_all` 與 `notifications::dispatch_all` 等 20 秒還追不上 outbox 時，錯誤訊息後面會附上 `metrics::describe_xid_holders()` 的輸出：
每一筆持有 xid 的交易的 `datname`、`pid`、`xact_start`、經過秒數、`state`、`wait_event`（`型別:事件`）、`application_name`、`query`（前 120 字），連同 prepared transactions。
例如 `wait_event=IPC:CheckpointDone`、`query="DROP DATABASE …"` 就是 DROP 在等 checkpoint。
訊息由 `metrics::horizon_timeout_message()` 組成，`t1_test_infra::horizon_timeout_message_names_the_holder`（G2）持有一個寫入交易，檢查訊息含它的 pid、datname，而且 query 截在 120 字。

## 等待一律有上限（T1b、G11）

測試裡的每一個等待都要有上限，逾時就 panic，訊息寫明在等什麼，並附上當下狀態。**新測試必須用共用的 wait helper**，不要自己寫 `loop { … sleep … }`，也不要直接 `.await` 一個可能永遠不會完成的東西。

| 要等的東西 | 用法 |
|---|---|
| fault gate 被走到 | `gate.reached.notified().await`：`Gate` 本身有上限（`Reached` 型別），逾時訊息帶 gate 的 point 名稱。要更短的上限用 `Gate::with_timeout(d)` |
| 輪詢某個條件（run 結束、有人在等鎖、F 越過某個 xid、轉換到某個狀態） | `common::poll_until(label, common::WAIT_TIMEOUT, async \|\| …)`，閉包回 `common::Break(值)`（成立）或 `common::Continue(看到什麼)`（還沒，逾時訊息會附上最後一次看到的） |
| 一個 `tokio::spawn` 出去的 task | `common::join(label, handle).await`：上限 `WAIT_TIMEOUT`；task 自己 panic 時原樣往外拋（`resume_unwind`），其他 `JoinError` 轉成帶 label 的 panic |
| 其他 future（`Notify`、channel、還沒 spawn 的請求 future） | `common::wait_for(label, common::WAIT_TIMEOUT, fut).await` |
| 同時跑幾個請求（`tokio::join!`）、`JoinSet` 收完所有 task | 整段包起來：`common::wait_for(label, common::WAIT_TIMEOUT, async { tokio::join!(a, b) }).await`；`JoinSet` 則把 `while let Some(r) = set.join_next().await { … }` 整個迴圈包進 `wait_for` |
| 搜尋或通知的下一輪「一定處理到」自己的列 | 先 `common::wait_horizon_past(&pool, xid)`：等 cluster 的 snapshot horizon H 越過自己的 xid，再停 gate 或只跑一輪 |

- 上限常數是 `oxledger::test_support::WAIT_TIMEOUT` = **60 秒**（`common::WAIT_TIMEOUT` 重新匯出）。規則是「該等待平常耗時的 10 倍以上，下限 30 秒」：這些等待平常不到 1 秒，全套平行時最慢到數秒（m1_10_ai_run 整檔曾從 50 秒變 350 秒），`sync_all`／`dispatch_all` 自己也會等 horizon 20 秒。輪詢間隔 `POLL_EVERY` = 20 ms。`Gate::default()` 的上限就是這個常數（`t1_test_infra::gates_wait_sixty_seconds_by_default`，不實際等 60 秒）。
- 逾時訊息（`test_support::timeout_message`）：`timed out after 60s waiting for <label> (wait at <檔案>:<行>:<欄>)`，接著
  - `outbox: H=… newest xid=… rows=…` 與每個 consumer cursor（`search@…`、`notify`）的狀態與之後還有幾筆 `issue` 列；
  - `metrics::describe_xid_holders` 的輸出（誰壓住 horizon）。
  狀態讀自 `common::test_pool()` 最後開的那個 DB（`test_support::set_report_pool`），用自己的連線讀，池被卡住的工作佔滿時也讀得到。每一步各自有上限，一步失敗只寫原因，不會蓋掉其他段：
  - 連線：先開一條單獨連線，3 秒（`REPORT_CONNECT_TIMEOUT`）；連不上時保留 sqlx 的錯誤（例如被拒），對方收了連線卻不回應時寫 `state: cannot connect: no answer within 3s`。不直接用池連，是因為池會把「連線被拒」重試到逾時，最後只剩 `PoolTimedOut`。
  - outbox、xid holders 兩段各 5 秒（`REPORT_SECTION_TIMEOUT`），連線另設 `lock_timeout` 2 秒、`statement_timeout` 4 秒。`outbox` 被 `LOCK TABLE` 鎖住時，outbox 段寫 `outbox: lookup failed: … lock timeout`，xid holders 段照印（`t1_test_infra::state_report_survives_a_locked_outbox`）。
  - 整份報告再有 15 秒的總上限。連線失敗兩種情況由 `t1_test_infra::state_report_says_why_it_cannot_connect` 檢查。
- **`(wait at …)` 指向呼叫的那一行**：`poll_until`、`wait_for`、`join`、`wait_horizon_past`、`Reached::notified` 都是 `#[track_caller]` 的一般函式，回傳 `impl Future`（`async fn` 不能加 `#[track_caller]`），呼叫時先記下 `Location::caller()`。自己包一層 helper 時也要加 `#[track_caller]` 並寫成回傳 future 的一般函式，否則位置會停在 helper 裡。
- **`poll_until` 的檢查本身 panic 也會附狀態**：檢查閉包裡的查詢失敗（例如 `PoolTimedOut`）或斷言失敗，會被接住，改成 `the check failed while waiting for <label> (wait at …): <原訊息>` 加上狀態再 panic（`t1_test_infra::a_failing_check_reports_the_state`）。所以輪詢裡的不變式斷言可以直接寫在閉包裡（例如 `s8_search_consistency::frontier_never_passes_a_partly_applied_xid` 的「F 不能越過還沒套完的 xid」）。
- 只跑一輪 `sync`／`sync_round`／`gc` 就對「處理到自己的列」下斷言的測試，在 cluster 上有別人的舊交易時會失敗（G21）。先 `common::wait_horizon_past(&pool, xid)` 再跑那一輪，或用 `poll_until` 重跑到條件成立，每輪都檢查不變式（`p2_search::outbox_gc_respects_leases`、`p2_search::empty_rounds_commit_the_payload_only_now_and_then`、`s8_gate::t_s2_outbox_consumed_twice`）。
- 輪詢不要用「最多 N 輪」當上限：horizon 被壓住時一輪可能什麼都沒做（G21），輪數和進度無關。用 `poll_until` 的時間上限。
- `t1_test_infra::bounded_waits_panic_with_the_state` 用 200 ms 的 gate、100 ms 的 `poll_until` 檢查兩種逾時真的會 panic、訊息含 label、呼叫位置、outbox 與 xid holders；`wait_horizon_past_waits_for_older_holders` 檢查有較舊的 xid 壓著時 `wait_horizon_past` 不會提早回來。
- 平常就短的等待不用改成 60 秒：測試刻意設的時間窗（例如「1500 ms 內不能回來」）是斷言，不是等待。只有「等某件事發生」才算等待。

### 例外（刻意不套 `WAIT_TIMEOUT` 的地方）

| 位置 | 理由 |
|---|---|
| 刻意的短時間窗：`p2_api_gaps` 的 5 秒、`m1_5_forms` 的 700 ms、`m1_9_ai_team` 的 400 ms「不能提早完成」檢查、`s8_search_consistency` 的等待者時間窗 | 這些是斷言（「這段時間內不能發生」），逾時是預期結果，不是卡住。時間窗之後真正等完成的那一步已改用 `wait_for`／`join` |
| `p2_tx_cancel` 的 `idle` 輪詢（200 µs 間隔、10 秒上限）與 `began`（1 ms 間隔、1 秒上限，逾時回 `false` 不 panic） | 已有自己的上限；它要抓很短的狀態，20 ms 的 `POLL_EVERY` 太粗。`began` 逾時代表「`BEGIN` 還沒送出」，呼叫端會再 poll 一次，最多試 10 次才 panic |
| `m1_8_notifications` 的效能寫入 task | 用 `wait_for(…, WAIT_TIMEOUT * 5, …)`：大量寫入本來就要數十秒，60 秒在滿載時太緊 |
| `s8_gate_extra` 的背景 task | 用 `wait_for(…, 120 s, …)`，理由同上 |
| `m1_11_e5` 的 harness（等 run 結束的迴圈） | 已有自己的輪數上限（每輪 100 ms），到上限回 `Err("run did not end")` 給 harness 計分，不 panic；`e5_live` 接真的模型，時間由外部服務決定 |
- nextest 的 `slow-timeout`（60 秒標示、10 分鐘砍）是最後一道防線，數值等 D3 量測後再定；有了 `WAIT_TIMEOUT`，卡住的測試會在 60 秒左右自己失敗並印出狀態，不會等到被砍。

## 效能不用 wall-clock 上限（T1b）

測試**不要用「花了多久」判斷效能**（`assert!(t.elapsed() < 500ms)`、產品的時間預算剛好夠用之類）。這台 Windows 主機 CPU 被吃滿時（全套 12 平行、或旁邊有編譯），每次 DB 往返從約 0.7 ms 變成約 31 ms，
一個 33 條 SQL 的請求就超過 500 ms，產品沒變、測試卻失敗（`m1_8_notifications` 的兩個測試就是這樣時好時壞；用 12 個忙迴圈壓 CPU 可以穩定重現）。

| 要驗的事 | 改用 |
|---|---|
| 「N 筆資料不會讓成本暴增」「這段是一條 SQL」 | 數 statement：`m1_8_notifications` 的 `statements(fut)` 在 `fut` 執行期間用 `tracing::subscriber::set_default` 掛一個只收 `sqlx::query` 事件的計數 layer（只影響這個執行緒，不換全域 subscriber），比較有、沒有那 N 筆時的數字（`v_watcher_count_with_many_invisible_watchers`） |
| 查詢計畫不會退化（全表掃描、巢狀迴圈） | `EXPLAIN (FORMAT JSON)` 檢查計畫節點或 cost 上限 |
| 產品有時間預算（例如 unread-count 的 2 秒），但測試要驗的是預算內的結果（上限、capped） | `test-support` 下的旋鈕把預算調大：`views::COUNT_BUDGET_MS`、`notifications::COUNT_BUDGET_MS`（0 = 正式值）。旋鈕只在 `#[cfg(feature = "test-support")]` 下存在，正式 build 的常數不變 |
| 要驗「逾時後怎樣」（stale、取消） | 用 `faults::Gate` 擋住，讓逾時一定發生，不要靠機器慢 |
| 取消要落在某個時間窗（例如 `BEGIN` 已送出、回應還沒讀） | 觀察狀態決定時機，不要掃時間點：`p2_tx_cancel::cancel_after_server_began` 在兩次 poll 之間從另一條連線看 `pg_stat_activity` 到 `idle in transaction` 才丟掉 future |

真的需要時間上限（例如量 p95）時，放進單獨跑的群組（`threads-required = 'num-test-threads'`，或 `#[ignore]` 另外跑），並在測試與 `.config/nextest.toml` 註明為什麼需要時間。
「這段時間內不能發生」的時間窗斷言（見上面的例外表）不是效能上限，不在此限。

## 連線數（D10）

- `OXLEDGER_DB_MAX_CONNECTIONS` 設定連線池上限；沒設時維持 sqlx 預設 10（正式環境不變，`db::tests::max_connections_default_is_unchanged`）。設成非正整數時程式啟動就 panic。
  套用的那段是 `db::pool_options_from(值)`，`db::tests::pool_options_apply_max_connections`（G3）不改環境變數，直接檢查池拿到的上限。
- 測試的 `test_pool()` 沒設這個變數時每個池 4 條（`common::TEST_MAX_CONNECTIONS`）。
- 估算：每個 slot 最多約 4（池）＋1（建 DB 用的管理連線）＋少數測試另開的 1–10 條；12 個 slot 約 60–180 條，在 300 以內。第二套測試開自己的容器，不算在這裡。

## Argon2（D4）

`test-support` 下密碼雜湊用 argon2 crate 接受的最低參數（m=8 KiB、t=1、p=1）。**測試不驗證正式環境的雜湊成本**；正式 build 維持 m=19 MiB、t=2、p=1，
由 `auth.rs` 的編譯期斷言與 `auth::tests::argon2_production_params_are_unchanged` 保證。驗證時參數讀自 PHC 字串，所以兩種雜湊在兩種 build 都驗得過。
`cargo test` 一定會開 `test-support`（crate 對自己的 dev-dependency），所以沒有測試跑得到正式分支；G1 改成在 `#[cfg(not(feature = "test-support"))]` 下對 `hash_params()` 做編譯期斷言：
把正式分支改成低參數時，不開 feature 的 `cargo check -p oxledger --no-default-features` 就會編譯失敗（T1 第三輪 mutation 實測）。
注意 E2E 的後端也是用 `--features test-support` 編的，所以 E2E 建的帳號同樣是低成本雜湊。

## `RUST_MIN_STACK`（D13）

不設 `RUST_MIN_STACK` 跑全套（`test-support` 與 `test-support,ai` 各一次）時，只有 `s5_changesets::s5_changesets` 堆疊溢位（debug build 下這個 future 超過 2 MiB 的測試執行緒）。
`m1_11_e5::e5_harness_self_check` 不設也能過；`e5_live` 要真的模型，這裡沒辦法跑。

處理方式：這兩個測試（`s5_changesets`、`e5_live`）改成 `#[test]`，用 `common::run_with_big_stack` 在 16 MiB 堆疊的執行緒上開 current-thread runtime 跑（和 `#[tokio::test]` 一樣）。
全套不設 `RUST_MIN_STACK`。以後有新的測試溢位（nextest 顯示 `ABORT … 0xc00000fd` 或 `has overflowed its stack`），同樣包一層，不要改回全域設定。

## Golden 測試（insta、proptest、mutation；決定 G1–G18 見 `decisions.tsv`，依據見 `docs/research/09b-golden-testing-review.md` §4）

### 有哪些快照

| 主題（`tests/snapshots/<主題>/`） | 測試 | 內容 |
|---|---|---|
| `errors` | `src/error.rs` 的 `error_code_table`（單元測試） | 全部錯誤碼（目前 73 個）依 code 排序：HTTP status、retryable、next_actions。`ErrorCode::ALL` 和 enum 的 JSON schema 比對，漏列一個 variant 就失敗，所以新增錯誤碼一定會進表。前端 `errors.ts` 的對照在前端測試 |
| `ai` | `m1_10_ai_run::t_m1_117_both_protocols_same_events`（`ai` feature） | 兩種 protocol 各兩輪送給 provider 的請求：path、幾個 header（key 遮成 `<secret>`）、body（system prompt、tools、歷史、max_tokens）。header 與 key 的明確斷言保留 |
| `markdown` | `m2_1_markdown::golden_markdown_examples` | `tests/golden/markdown/*.md` 每檔一個快照（`render@<檔名>.md.snap`）：Markdown（人、agent）與 plain 三種的 stage 1 HTML 與 refs；每個輸出也過 `assert_safe` |
| `search` | `m1_7_search::golden_ranking_on_a_fixed_corpus` | `tests/golden/search/corpus.json` 的固定語料與查詢：名次、key、strength、命中欄位、subject（API 沒有分數，不用四捨五入） |
| `schema` | `golden_schema::schema_catalog` | migration 建出的 schema，查 `pg_catalog` 後在 Rust 排序：`migrations`、`columns`、`constraints`、`indexes`、`triggers`、`functions`（函式本體只放 12 字的 md5）、`types` |

- 斷言一律具名，用 `common::snap::settings("<主題>").bind(|| insta::assert_snapshot!("<名稱>", …))`（`src` 的單元測試直接寫 `with_settings!`，路徑指到 `../tests/snapshots/<主題>`）。快照路徑與名稱不跟著 module 或 binary 走，D6 合併 test binary 時不用搬。
- 語料放 `tests/golden/<主題>/`，用 `insta::glob!` 跑；`INSTA_GLOB_FILTER`（或 `cargo insta test --glob-filter`）可以只跑部分語料。
- `schema_catalog` 用 `<slot DB>_schema`：每次 `DROP SCHEMA public CASCADE` 後重跑 migration，不是 `DROP DATABASE`，所以不在 `drop-database` 群組；別的測試留在 slot DB 的東西（`issue_cf_f<n>` 動態索引、失敗時沒刪掉的 trigger）不會進快照。

### 正規化（`tests/common/snap.rs`）

拍之前先過 `Normalizer`，**保留參照關係**，不要把不確定的值全抹成同一個字串：

| 值 | 規則 |
|---|---|
| uuid（值、字串裡、物件 key，大小寫都算） | 依第一次出現的順序 `<id1>`、`<id2>`…；同一個 id 永遠同一個標籤。已知的實體先 `name(id, "user:alice")` 得 `<user:alice>` |
| 時間（RFC 3339、PostgreSQL 的空白格式） | `<ts>`；只有日期的字串保留 |
| `xid`、`*_xid` 的 key | 刪掉；其他時序值（frontier、generation）用 `drop_key` 刪，或乾脆不放進快照 |
| 浮點數 | 四捨五入，預設 4 位（`float_digits`） |
| 集合語意的陣列 | `set_key("tags")`，正規化後排序 |
| 密鑰 | `secret(KEY)` 遮成 `<secret>`；`pretty()`／`assert_no_secrets()` 另外斷言原值不在輸出 |

- 正規化函式的測試在 `t1_test_infra` 的 `snap_*`（放在一個 binary，不會在每個 include `common` 的 binary 各跑一次）。
- id 第一次出現的位置要是確定的（不要先出現在一個沒排序的集合裡），否則標籤號碼會變。
- 快照只拍「這個測試要守的那一塊」，單檔超過約 200 行就考慮拆（`schema` 的 `columns` 例外：一行一欄，拆開反而難審）。

### 換行（G17）

insta 比較前會把 `\r\n` 換成 `\n`、去掉結尾空白，所以**快照看不出換行差異**。和 CR 有關的行為一律用明確斷言（`m2_1_markdown::cr_line_endings`），語料檔一律 LF（`golden_markdown_examples` 讀到 `\r` 就失敗）。
`.gitattributes` 明列 `*.snap text eol=lf`、`*.aria.yml text eol=lf`；XSS 語料（含控制字元、RTL override）留在 Rust 常數，不放進文字檔。

### 本機：新增或更新快照

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
cargo nextest run -p oxledger --features test-support,github,ai -E 'binary(m2_1_markdown)'   # 不符：失敗並留下 .snap.new
cargo insta review                                                                         # 逐一看 diff，a 接受、r 拒絕
cargo insta test -p oxledger --test-runner nextest --dnd --features test-support,github,ai --review --test m2_1_markdown   # 一次收集多個再審（cargo-insta 不能轉交 nextest 的 -E；用 --lib／--test 選 binary，`--` 之後放測試名稱）
```

- `.snap.new`、`.pending-snap` 在 `.gitignore`，不會被 commit。
- 不在任何腳本寫 `INSTA_UPDATE=always`、`cargo insta accept`、`--accept`。新快照第一次產生後，人工看過內容合理才接受；看到像 bug 的值要回報，不要直接接受。
- **新快照在不同 slot 連跑兩次**確認穩定（G16），之後才 commit。
- PR 說明列出每個改動的快照與原因（新增、修改分兩節），說不出原因就是 bug 或正規化不足。

### 合流（G5）

```powershell
$env:CI = 'true'
cargo insta test -p oxledger --test-runner nextest --nextest-profile merge --dnd --features test-support,github,ai --unreferenced=reject
Remove-Item Env:CI
```

- `CI=true` 時 `cargo insta test` 等同 `--check`：不寫任何檔，不符、**缺少**的快照都失敗。
- `--unreferenced=reject`：全部測試通過後，`crates/oxledger` 底下沒被任何測試用到的 `.snap` 讓這次失敗。**只在全套時用**（只跑部分測試時沒跑到的快照會被當成孤兒）；**不要設 `auto`**（非 CI 時會刪檔）。
- features 一定要 `test-support,github,ai`：`m1_10_ai_run` 等檔有 `#![cfg(feature = …)]`，沒編進來時它們的快照會被當成孤兒。`#[ignore]` 或需要外部資源（真的模型、key）的測試不擁有快照。
- `--dnd`（`--disable-nextest-doctest`）：不然 cargo-insta 會另外用 `cargo test --doc` 跑 doctest，繞過 nextest 的逾時。
- `merge` profile（`.config/nextest.toml`）繼承 default 的群組與 overrides，只固定 `retries = 0`、`flaky-result = "fail"`（以後開重試時，重試才過的測試在合流仍算失敗）、`fail-fast = false`。
- 2026-11-14 實測：用上面的合流指令只跑本次的快照測試（`--lib --test golden_schema --test m1_10_ai_run --test m1_7_search --test m2_1_markdown -- error_code_table golden_ t_m1_117 schema_catalog`）全過、`no unreferenced snapshots found`；在 `snapshots/errors/` 放一個多餘的 `.snap` 時以 `aborting because of unreferenced snapshots` 失敗。
- 合流前 `git status --porcelain` 不得有 `*.snap.new`、`*.pending-snap`。
- OpenAPI golden（`api/openapi.json`、`api/types.ts`）維持原機制，`CI` 有設時忽略 `OXLEDGER_UPDATE_API_SPEC`，過期就失敗（`s2_api::spec_update_allowed`，判斷和 insta 的 `CI` 一致）。

### proptest（G13）

| 不變式 | 位置 |
|---|---|
| filter AST（`FilterInput`）序列化成 JSON 再 `parse_input` 回來不變 | `src/filter.rs` 的 `prop_filter_input_round_trips` |
| 任何文字、兩種格式、人或 agent，stage 1 都過 `assert_safe`（允許清單），而且結果確定 | `m2_1_markdown::prop_stage1_is_always_safe` |
| visibility：`simplify` 不改語意且冪等；`can_see` 等於模組文件寫的 Redmine 規則（另寫一份不經 `Pred` 的參考實作）；匿名看不到私有、多一個角色不會少看到 | `ox-core` `src/visibility.rs` 的 `prop_*` |

- 案例數 256–512，整組不到幾秒；失敗的反例存在 `crates/oxledger/proptest-regressions/`、`crates/ox-core/proptest-regressions/`，**要 commit**（下次先重跑這些反例）。
- 反例修好後也可以變成 golden 語料或明確斷言。

### Mutation（G12）

設定在 `.cargo/mutants.toml`：`test_tool = "nextest"`、`additional_cargo_args = ["--profile", "mutants"]`（**nextest** 的 profile，`fail-fast = true`；不是 cargo-mutants 自己的 `--profile`，那是 cargo 建置 profile）、`features = test-support,github,ai`。

```powershell
git diff main > $env:TEMP\d.diff
cargo mutants --in-diff $env:TEMP\d.diff -j1 -- -E 'binary(m2_1_markdown)'
```

- 只在 diff 範圍跑，`-j1`；等 D2 每個測試都有自己的 slot DB 之後再開平行。`--in-diff` 只選 mutant，不縮小測試範圍，所以要自己用 `-E` 過濾。
- 不擋合流。只改測試的 diff 不會產生 mutant。nextest 不跑 doctest，只被 doctest 抓到的 mutant 會顯示 missed。
- `cargo mutants --list` 不跑測試，可以用來確認設定讀得到（golden 導入時只跑過這個，還沒有實際跑過 mutant）。設定裡的 `--profile mutants` 也會傳給 `cargo check --tests`，所以不要用 `cargo mutants --check`。

### 覆蓋對照（G18）

cargo-llvm-cov 0.9.1 與 rustup `llvm-tools` 已裝。D2 之後每月或每個里程碑跑一次全套覆蓋，產生「原始碼檔 → 測試」的對照，校正 T7 的選測試對照表；不做即時的 coverage 選測試。一次約一輪全套的時間。

## 舊跑法（只在必要時）

```powershell
$env:OXLEDGER_TEST_DATABASE_URL = 'postgres://oxledger:oxledger_dev@127.0.0.1:55432/oxledger_test_be'
cargo test -p oxledger --no-fail-fast -- --test-threads=1
```

單執行緒、共用一個 DB。開發用的 `oxledger-pg` 上有 E2E 與開發伺服器的交易，G21 逾時比較容易出現；而且升級測試會在那台 server 上建、刪暫時 DB。

## 實測（2026-11-14，12 邏輯核心，Windows 11，Docker Desktop）

| 跑法 | 測試數 | 時間 |
|---|---|---|
| 舊：`cargo test --test-threads=1`，`oxledger-pg` 上的 `oxledger_test_be`，`RUST_MIN_STACK=16777216`，T1 之前的程式（不含 16 個 DROP DATABASE 測試） | 475 過、1 ignored | 17 分 51 秒（各 binary 回報的執行時間合計 905 秒） |
| 新：`cargo nextest run -p oxledger`，測試容器，12 平行 | 493（491 過、2 失敗、1 skipped） | nextest 342 秒；含 cargo 檢查與列測試 6 分 08 秒 |
| 新：`--features test-support,ai` | 523（521 過、2 失敗、2 skipped） | nextest 360 秒 |
| 第二輪（修好兩個失敗、SCRAM opt-level）：`cargo nextest run -p oxledger` | 494（全過、1 skipped） | nextest 321 秒（`cargo clean -p oxledger` 後含完整重編共 13 分 23 秒） |

前後不是同一個環境：舊數字是在開發用的 `oxledger-pg`（有 fsync，同時還有 E2E 與開發伺服器的負載）上量的，新數字是在 `fsync=off`＋tmpfs 的測試容器上量的。
所以「快幾倍」裡混有環境差異，不全是平行與 slot DB 的功勞；同環境的比較只有下面 D3 那張表（兩邊連線、migration 檢查差不多，TRUNCATE 與寫入快 10–30%）。

D3（改之前，`s6_read` 10 個測試、每個 `World::build` 的平均）：

| 步驟 | 開發用 PG | 測試容器 |
|---|---|---|
| 管理連線＋查 `pg_database` | 80 ms | 80 ms |
| 開連線池（第一條連線） | 78 ms | 77 ms |
| migration 檢查（`MIGRATOR.run`，都已套用） | 11 ms | 11 ms |
| `TRUNCATE` 約 50 張表 | 172 ms | 154 ms |
| `UPDATE org_settings`＋`reconcile_indexes` | 19 ms | 13 ms |
| Argon2 雜湊一次（19 MiB） | 21 ms | 21 ms |
| `create_user` ×3 | 97 ms | 88 ms |
| `sign_in`（含一次 Argon2 驗證） | 48 ms | 37 ms |

兩次連線（約 160 ms）和 TRUNCATE（150–170 ms）是準備成本的大宗；Argon2 四次約 85 ms。

### SCRAM 相依的 opt-level（T1 第二輪）

debug build 下 sqlx 登入 PostgreSQL 的 SCRAM-SHA-256（4096 輪 HMAC-SHA-256）跑在沒最佳化的 `sha2`／`hmac` 上。根目錄 `Cargo.toml` 對 `sha2`、`hmac`、`digest` 設 `opt-level = 3`（Cargo.lock 裡沒有 `pbkdf2`，sqlx 自己跑迭代）。
量法：暫時的測試對測試容器連線 30 次取平均，`test_pool()` 開關 10 次取平均，各跑 3 次：

| | 單一連線 | `test_pool()`（管理連線＋池） |
|---|---|---|
| 改之前 | 73.4–74.7 ms | 165.6–166.6 ms |
| `sha2`／`hmac`／`digest` opt-level 3 | 38.1–38.8 ms | 92.9–97.7 ms |
| 再加 `sqlx-postgres` opt-level 3 | 38.4–38.5 ms | 91.8–92.9 ms（沒有差別，已撤回） |

每個測試的準備省下約 70 ms。剩下的 38 ms 不在這幾個 crate（可能是伺服器端 SCRAM 與 Docker 的網路轉發），沒有再追。

### 平行時曾經失敗、第二輪已修的兩個

- `m1_9_ai_team::t_m1_121_one_runtime_per_client`：產品的死結。`ai_team::create` 先插 `operation_requests`（FK 對 sponsor 的 `principals` 列拿 FOR KEY SHARE），才拿 sponsor 的 advisory lock；`agent_principals_runtime_guard` trigger 之後要 `FOR UPDATE` 同一列。兩個 create 同時進來時，一方拿著 advisory lock 在 trigger 等對方的 KEY SHARE，對方拿著 KEY SHARE 在等 advisory lock。修法：`serialize_sponsor` 移到交易一開始、任何列鎖之前（固定鎖順序），輸的那方照規格回 `runtime_client_taken`。回歸測試 `t1b_concurrent_creates_never_deadlock` 用 fault gate 讓兩個請求同時停在 `ai_team.create.begun` 再一起放行，跑 20 次。
- `s5_changesets::s5_changesets`（T-R5「session held」）：測試的時序假設。交易 B commit 之後，等著的 session 撤銷馬上 commit，接著 `finished()` 用已撤銷的身分讀結果失敗，照 §8.6（decisions S5「finished() 讀取失敗一律回 202」）回 `pending`。開發用 PG 有 fsync，撤銷 commit 比較慢，讀取剛好搶在前面。測試改成用 `apply.after_commit` gate 固定「撤銷先 commit、再讀」的順序，斷言回應是 202、`get_execution` 看到 `succeeded`、note 多一筆；「撤銷在等鎖」改成查 `pg_stat_activity` 看到撤銷的 UPDATE 在等 Lock，不再 sleep 300 ms。
