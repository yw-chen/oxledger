# P2 搜尋收尾：檢查清單與疑點

範圍：設計 03 §7 第 7–11 步，加上 S8 延後的 §17.15 索引 fast field（decisions.tsv 第 258 列）與 §4.2 的 durable payload（第 247、261 列）。

## 階段

| 階段 | 內容 | 完成標準 |
|---|---|---|
| P2.1 | generation 目錄與 durable payload（03 §4.2 第 5 步、重啟恢復表）；索引 schema v2：`search_rev` STORED、fast field `fixed_version`、`start_date`、`due_date`、`nulls` | 重啟後 F 從 payload 恢復、PG cursor 落後或領先時以 payload 為準重播；payload 無效或版本不符時觸發重建；空 commit 有節流 |
| P2.2 | 藍綠重建（03 §4.8）；`outbox_cursors.state`；outbox 清理（§4.9）；移除舊 `rebuild()` | §4.11 藍綠測試：租約和 GC 互搶、F 相同 cursor 不同不切換、舊 token 切換後仍有效、F 不倒退、舊 token 不會在空索引上被滿足、重建期間搜尋不中斷 |
| P2.3 | fast field 下推（`version`、`overdue`、Filter AST 內建欄位）；evidence 用 `search_rev`（第 9 步） | 下推只縮小候選、SQL 仍複驗；有 `q` 加選擇性高的版本條件時不再 truncated；`search_rev` 相同與不同兩條路結果一致 |
| P2.4 | 指標（§4.6）與長交易告警（§4.7）；`/metrics`；同步停住超過 60 秒回 `search_unavailable` | 指標在 `/metrics` 看得到；重現跨 database 的已分配 xid 交易時告警觸發 |
| P2.5 | D14 策略選擇（§5.4）：探測、S0／S2、共用 deadline、`query_timeout` | §5.8 的策略、計畫結構、deadline、分頁測試 |
| P2.6 | `resolve(kind: "custom_field")`：型別、單位、選項、運算子、`definition_version`；篩選帶舊版本回 `field_definition_changed`（第 11 步） | 用法 C 的 E2E |

## 疑點與照建議的做法

- P2-D1 **索引目錄**：`<OXLEDGER_INDEX_DIR>/CURRENT` 加 `gen-<uuid>/`。舊版（S8 以前）把索引直接放在根目錄，沒有 `CURRENT`；升級後當成「沒有 generation」，用快照重建，舊檔案留著不刪（寫進 upgrade-notes）。
- P2-D2 **沒有可服務的 generation 時**（第一次啟動、payload 壞掉、`data_epoch` 不符）：有 `q` 的搜尋回 `search_unavailable`（retryable），不回空結果假裝沒有；純 SQL 搜尋照常。帶 token 的等待照常落後（F 是 0），不會在空索引上被滿足。
- P2-D3 **測試用記憶體索引**（`SearchIndex::open(None)`）走同一條路：第一次 `sync` 先做快照重建，再照常同步。
- P2-D4 **空 commit 節流**：只有 F 前進、沒有文件變動時，距上次 durable commit 超過 10 秒才寫 payload（03 §4.2）。測試可以用 `set_durable_interval` 改成 0。
- P2-D5 **PG cursor**：`outbox_cursors` 改用 `consumer = 'search@<generation>'`，只用來協調、監控與 GC；恢復一律以 payload 為準。舊的 `consumer = 'search'` 那列由 migration 刪掉。
- P2-D6 **切換**（Q-S8 建議預設）：在同步輪的鎖裡做，舊 generation 這時不會前進；新 generation 要 `F_new ≥ F_old` 而且 cursor ≥ 舊 cursor，不夠就在鎖裡繼續追。健康檢查：baseline 完成時文件數等於同一快照的未封存 issue 數；切換前抽 200 筆比對 `search_rev`，不一致的要在新 generation 的 cursor 之後有 outbox 事件，否則中止切換。
- P2-D7 **analyzer／查詢語意版本**：目前只有一版（analyzer 1、query semantics 1、schema 2）。「重建期間舊 generation 用舊版 tokenizer」要等真的改版時才需要；現在版本不符就重建，舊 generation（版本不同）不服務。寫進 decisions。
- P2-D8 **保留舊 generation**：切換後狀態 `retained`、保留 24 小時（`retained_until`），期間不同步；清理工作刪目錄與 consumer 列。回切要先重新追上，第一版不提供回切指令。
- P2-D9 **outbox 清理**：每小時一次，界線照 §4.9；consumer 的狀態只有 `active`、`bootstrapping`、`retained`、`retired`，`retired` 不擋清理。刪 consumer 只在明確的管理動作（保留期滿或重建失敗）時做。
- P2-D10 **`/metrics`**：Prometheus 文字格式，自己產生，不加依賴。只有設定 `OXLEDGER_METRICS_TOKEN` 時才開，要 `Authorization: Bearer <token>`；沒設定時回 404。`bound="commit"` 只在 `track_commit_timestamp = on` 時記錄。
- P2-D11 **`search_unavailable` 的「同步停住」**：只在背景同步迴圈有在跑時判斷（手動同步的測試不受影響）；上次成功超過 60 秒就回。
- P2-D12 **fast field 下推只做超集**：Filter AST 的 `or` 只要有一個子條件不能下推，整個 `or` 不下推；`ne`、`is_null`、`not_null` 只在條件的 applicability 是 `True` 時下推。下推的結果一定包含 SQL 會留下的列。
- P2-D13 **`search_rev`**：`sha256("v1" ‖ u64be(len(subject)) ‖ subject ‖ u64be(len(description)) ‖ description)`，存成 STORED bytes。相同時 `evidence_status: "current"`，不同時重新比對原文、`"rechecked"`。
- P2-D14 **D14 策略**：03 §5.4 說現行 `updated_at DESC` 排序要先補 `(updated_at, id)` 索引與實測才能用 S2。照建議預設：補 `issues (updated_at DESC, id DESC)` 索引，S2 沿這個索引反向走；先在 `experiments/sd` 的資料上量 S0／S2，量不出改善就只做 deadline 與 `query_timeout`，策略固定 S0，並記錄。**實測結果（2026-11-09，experiments/sd README 末節）**：寬條件 S2 快 10–160 倍，稀有條件 S0 快，策略選擇保留。
- P2-D15 **`definition_version`**：`field_defs` 加 `definition_version`（從 1 起）與 `unit`（可空）；選項新增、改名、停用、刪除，或改 `unit`、`ftype`，觸發器讓它 +1（建立欄位的同一個交易裡寫的選項算第一版，不 +1）。Filter AST 條件可帶 `definition_version`，不符回 `field_definition_changed`（409，不可重試，`next_actions: ["fix_input"]`，details `field`、`current_version`、`suggested_tool: "resolve"`）。

## 獨立驗證（2026-11-09）

子代理在自己的副本與 DB（`oxledger_be_verify_p2`，已清掉）上檢查，找到 3 個缺陷、1 個文件矛盾、9 個存活的 mutation。處理：

| 項目 | 處理 |
|---|---|
| D1 切換時 `mark_active` 失敗會退役服務中的新 generation、刪掉目錄 | 改成先在 PG 標 active，成功後才寫 `CURRENT`、換 active、發布；PG 失敗時什麼都沒切換，舊的照常服務。`CURRENT` 寫入失敗只記錯誤（重啟時舊 generation 會重新標成 active）。測試 `a_failed_switch_keeps_the_old_generation` |
| D2 啟動時一次暫時性錯誤就丟掉磁碟上的 generation | `loaded` 等到真的服務才清掉，失敗時把開好或建好的 generation 放回去。測試 `a_failed_activation_retries_the_same_generation` |
| D3 沒有 current key 的 issue 讓 baseline 永遠失敗 | 計數改用和掃描同一個集合（`DOC_SELECT`），差額記警告。測試 `a_keyless_issue_does_not_block_the_baseline` |
| D4 升級說明叫維運先 `CREATE INDEX CONCURRENTLY`，migration 卻會撞名 | 0018 改成 `CREATE INDEX IF NOT EXISTS`，升級說明同步 |
| `reindex` 提示字串夾空白 | 改成換行 |
| M1 GC 界線 `<` → `<=` | `gc_keeps_the_cursor_transaction`（mutation 確認會失敗） |
| M2 payload 版本不檢查 | `a_payload_of_another_version_is_rebuilt`（三個版本欄位） |
| M3 預算之後的 `statement_timeout` | 改成記住原值、用 `set_config(..., true)` 還原（`ROLLBACK TO` 本身會還原）；沒有加外部可觀察的測試 |
| M4 健康檢查一律通過 | `an_unexplained_mismatch_aborts_the_switch` |
| M7 同步停住 60 秒 | `a_stalled_sync_makes_text_search_unavailable` |
| M8 租約起點 | `gc_during_bootstrap_keeps_replay_rows` 加斷言：租約不超前於當時服務中的 cursor |
| M9 overdue 邊界 | `fast_fields_narrow_candidates` 改用組織時區的今天：昨天到期算逾期、今天到期不算，另加 `due_date ge today` |
| M11 `publish` 的單調 | 未加測試：F 只由同步輪與切換（已要求 `F_new ≥ F_old`）推進，拿掉 `max` 在現有路徑不會倒退，留作防禦 |
| M12 切換前最後一次 commit | 未加測試：少了它只影響空輪推進的 F，重啟時 F 會從 payload 的 cursor 重新算，不會漏事件 |

驗證者另外提到、沒有改的：2 秒預算套用在所有沒有 `q` 的篩選列表（不只策略選擇的情形），`query_timeout` 訊息寫「field conditions are too broad」——照 03 §5.4 第 4 點「一個請求一個 SQL 預算」理解，保留；`track_commit_timestamp = on` 的 `bound="commit"` 路徑沒有實測（共用容器不能開）。
