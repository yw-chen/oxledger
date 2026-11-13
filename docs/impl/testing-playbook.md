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
  名單：`p2_search`、`s8_gate_probes`、`s8_search_consistency` 全檔；`s7_redaction::search_never_shows_redacted_text`、`s8_gate::t_s2_outbox_consumed_twice`、`m1_7_search::legacy_generation_on_disk_is_opened`、`m1_8_notifications` 的 5 個（見 nextest.toml）。
- **長時間持有 xid 的測試**：`p2_strategy` 的 2 個、`m1_2_views` 的 2 個一次交易寫上萬筆 issue，平行時要 20–40 秒，會讓旁邊所有 `sync_all` 等滿 20 秒逾時（s1_identity 就這樣失敗過，`describe_xid_holders` 直接指出是 `UPDATE projects SET issue_counter = issue_counter + 12000`）。
- **`t1_test_infra::horizon_timeout_message_names_the_holder`**（G2）：在 cluster 最老的 5 個 xid 持有者裡找自己的交易，平行時會被別的測試的交易擠掉。

這些測試用 `threads-required = 'num-test-threads'`：等其他測試都結束才開始，跑的時候沒有別人。
用 `cargo nextest list -E <兩段 filter>` 核對（T1 第三輪）：共 44 個，43 個會執行，另外 1 個是 `#[ignore]` 的 `m1_8_notifications::perf_dispatch_p95`（平常不跑）。
其中 T1 原本的 42 個可執行，加上第三輪新增的 G2 測試。合計約 100 秒（單獨跑時每個 1–2 秒，p2_strategy 各約 20 秒）。
新寫的測試如果直接呼叫 `sync`／`sync_round`／`dispatch_round` 或 fault gate，要用 `_all` 版本，或加進這份名單。

暫時保留單獨跑的原因：這些測試斷言的是「worker 走一輪就處理到」或「gate 一定等得到」，正確性建立在「cluster 上沒有別人的寫入交易」這個前提。
平行時這個前提不成立，失敗的是測試的假設，不是產品（H 被別的交易壓住時延後處理是 03 §4.7 的設計）。改寫要逐一確認每個斷言的意圖，T1 範圍內先隔離，不改斷言。

### 待辦：改寫成可以平行的測試

- [ ] `p2_search`、`s8_search_consistency`、`s7_redaction::search_never_shows_redacted_text`、`m1_7_search::legacy_generation_on_disk_is_opened`：改成迴圈等明確條件（`frontier() > X`、自己的列已處理），像 `s8_search_consistency` 的 `catch_up`，或改用 `sync_all`。
- [ ] `m1_8_notifications` 的 5 個：改用 `dispatch_all` 或等自己的 outbox 列被消耗；`perf_dispatch_p95` 要另外想量測方式（平行時的 p95 本來就不準）。
- [ ] `s8_gate::t_s2_outbox_consumed_twice`、`s8_gate_probes`：gate 等待加上「H 已越過自己的列」的前置條件，不要假設下一輪一定處理到。
- [ ] `p2_strategy` 的 2 個、`m1_2_views` 的 2 個：把一次寫上萬筆的交易拆小，或改用 `COPY`／分批 commit，讓 xid 不要一次持有 20–40 秒。
- 完成一項就把它從 `.config/nextest.toml` 的 override 拿掉，平行跑全套三次都過才算。

## 逾時時印出是誰壓住 horizon（D1(d)）

`SearchIndex::sync_all` 與 `notifications::dispatch_all` 等 20 秒還追不上 outbox 時，錯誤訊息後面會附上 `metrics::describe_xid_holders()` 的輸出：
每一筆持有 xid 的交易的 `datname`、`pid`、`xact_start`、經過秒數、`state`、`wait_event`（`型別:事件`）、`application_name`、`query`（前 120 字），連同 prepared transactions。
例如 `wait_event=IPC:CheckpointDone`、`query="DROP DATABASE …"` 就是 DROP 在等 checkpoint。
訊息由 `metrics::horizon_timeout_message()` 組成，`t1_test_infra::horizon_timeout_message_names_the_holder`（G2）持有一個寫入交易，檢查訊息含它的 pid、datname，而且 query 截在 120 字。

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
