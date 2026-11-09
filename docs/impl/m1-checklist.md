# M1 可日用：檢查清單與疑點

範圍：設計 05（`docs/design/05-m1-daily-use-contract.md`）§8 的 M1.0–M1.11。本文沒寫的共通規則沿用 05 與 02。

## 階段

| 階段 | 內容 | 完成標準 | 狀態 |
|---|---|---|---|
| M1.0 前置 | 0019、0020；`perm.rs` 權限清單；`estimated_hours` 進命令引擎、describe、事件、undo、政策；`issue_field_changes` 寫入與回填；D12 遮蔽刪投影 | 既有測試全綠；`estimated_hours` 走完 02 §16.7b 對應案例；T-M1-03 | 完成（2026-11-09，全套兩輪全綠，未 commit） |
| M1.1 Filter AST v2 | `filter.rs` 擴充成 v2（內建欄位、`me`、相對日期、`ever_in`、排序）；舊參數轉 v2；`/filter-fields`、`describe filter_fields`；`search` 收 `where` | T-M1-01…05；E4 可見性測試加 v2 隨機條件；OpenAPI 與 `api/types.ts` 重產 | — |
| M1.2 視圖與看板 | 0021；`/views*`；內建視圖；`run`／`preview`；看板計數；專案分頁 `view`、`board`、`triage`；MCP `view_list`、`view_run` | T-M1-06…20；前端可接列表與看板 | — |
| M1.3 Triage | 0022（含 `self_triage`、`same_sponsor`）；建立時進收件匣；`triage_decide`；設定；`/triage*`；Filter `triage_self_decided`；MCP `triage_list`；政策與委派頁「新操作」 | T-M1-21…30c；收件匣「最近決定」可用 | — |
| M1.4 工時 | 0023（含 `read_own_time`）；活動與設定；`log_time`／`update_time_entry`／`delete_time_entry`；日合計與鎖順序；可見性；報表、週表；agent 限制、預設 review、`time_list` 與 A1(c) 例外；委派精靈警告；undo、D12 | T-M1-31…44、39a、39b；鎖順序壓力測試不死結；警告文案出現在委派精靈（前端截圖檢查） | — |
| M1.5 表單與欄位管理 | 0024、0025；L1 規則進 `evaluate()`（所有入口）；L2；草稿／impact／發布／回溯；`describe_form`、`validate`、`/commands/validate`；`expected_rules_version`；自訂欄位管理與 Q-S9 轉換；conformance 向量 | T-M1-45…59 | — |
| M1.6 首頁與介面角色 | 0026；`/me/*`、範本；首頁小工具資料 | T-M1-60、61 | — |
| M1.7 搜尋 | 0027；BM25F 欄位與 fast field；別名展開；概念管理；藍綠重建切換 | T-M1-62…65；E1 nDCG＠10 不低於 0.810；E2 用獨立查詢集量別名增益（只記錄） | — |
| M1.8 通知 | 0028；`notify` consumer；`/notifications*`、`/issues/{key}/watch`、`/me/notification-preferences`；GC 與 lag 監控；保留清除 | T-M1-70…83、125、126、132；派送延遲 p95 ≤ 10 秒（本機 1 萬張單、50 人）；前端鈴鐺、通知中心、關注、偏好可用 | — |
| M1.9 AI 團隊 | 0029；runtime、角色 principal、共用脈絡；token 綁 principal；principal／專案層暫停；鏈上限；`/ai-team*`、`/me/agent-runtimes*`；角色範本；`describe agent_context` | T-M1-84…99、66a、121、123、124、127、128、133；02 的 E4w、§9.6 受眾測試、§16.7b 全部重跑全綠；既有 agent token 升級後照常可用 | — |
| M1.10 右欄 AI 對話 | 0030；feature `ai`；部署端點登記與 IP 驗證；兩種 provider 串流；執行層；`AuthMethod::BuiltinRun`；工時汙染；`/ai/*`、`/admin/ai/*`；D12 與保存清除；假 provider 測試伺服器 | T-M1-100…118、119、120、122、129、130、131、134；前端右欄對話與設定頁可用；真的 Anthropic key 與自架 OpenAI 相容端點各手動跑一次並記錄 | — |
| M1.11 發布門檻 | E4w 對新命令與新身分全套、T-M1-66…69、E5 子集、通知與 AI 對話的 canary 洩漏測試；T-M1-119、120、121、122、127 | 越權、繞過政策、重複效果、canary 洩漏 = 0；key 送往非登記主機 = 0、內建 AI 讀工時後的 direct 寫入 = 0；核心情境正確率 ≥ 90%；`upgrade-notes.md` 寫好 0029、0030 | — |

## M1.0 疑點與照建議的做法

- M1.0-D1 **`estimated_hours` 的正規形式**：命令文件裡收十進位字串或 `null`（02 §6.1 數字規則）；正規化沿用 `canonical_number`（去掉前導零與小數尾零，`"1.50"` → `"1.5"`，`"0.00"` → `"0"`）。事件 payload、`execution_field_changes`、no_effect 比對、`GET /issues/{key}` 都用這個形式；DB 讀出用 `trim_scale(estimated_hours)::text`。範圍 0–99999.99、最多兩位小數，超出回 `invalid_value`（`details.kind = "out_of_range"`／`"too_many_decimals"`）；不是十進位字串（含 JSON 數字、`""`、指數、`"1."`、`".5"`、`"+1"`）回 `invalid_request`。不四捨五入。語法檢查不設長度上限：不算數的零（前導零、小數尾零）先去掉再判範圍與小數位，所以 `"1.00000000000"` 收成 `"1"`、`"1.123456789012"` 回 `too_many_decimals`、21 位整數回 `out_of_range`（獨立驗證 D-1 修正）。
- M1.0-D2 **`role_permissions_tracker_scope` 與既有資料**（05 附錄 C R17，建議未處理）：新 CHECK 用 `NOT VALID` 加上，migration 內若沒有違反的列就立即 `VALIDATE`；有違反的列（非 issue 類權限限定了 tracker）就保留 `NOT VALID`、發 `NOTICE`，既有列的行為不變（不擴大、不縮小權限），新寫入與更新一律受檢。寫進 upgrade-notes。
- M1.0-D3 **既有測試 `s6_epoch` 用 `manage_versions` 測「限定 tracker」的 epoch bump**：新 CHECK 不允許非 issue 類權限限定 tracker。測試改用 `edit_own_issues`（issue 類、Dev 角色原本沒有），測的仍是「`role_permissions.all_trackers` 與 `role_permission_trackers` 改變會 bump 全部專案」。
- M1.0-D4 **0019 不給任何角色預設工時權限**：05 §2.2 的 DDL 沒有；給內建角色加權限會觸發全部專案 epoch（R17），屬於 M1.4 要和使用者確認的升級影響。M1.0 只加權限名稱與 CHECK。
- M1.0-D5 **`issue_field_changes` 的對應規則只有一份**：0020 定義 SQL 函式 `issue_field_change_rows(kind, payload)`（`created` 的 `fields` 初始值當 `new_ref`、`fields_changed` 的 `changes`、`status_changed` 的 `old`／`new`；只收 `status_id`、`assigned_to_id`、`fixed_version_id`、`priority_id`）。回填與命令引擎都呼叫它；引擎在寫 `issue_events` 的同一個 statement（data-modifying CTE）裡一起寫投影，`at` 等於事件的 `at`。已遮蔽的事件不回填。
- M1.0-D6 **D12 刪投影**：遮蔽 issue 事件（`Target::IssueEvent`）的交易內 `DELETE FROM issue_field_changes WHERE event_id = …`。遮蔽 note 不動投影（note 事件沒有投影列）。
- M1.0-D7 **T-M1-03 在 M1.0 的驗法**：`ever_in` 是 M1.1 的 Filter 運算子，M1.0 還沒有。M1.0 的測試直接用 05 §3.2 定義的語意（目前值在集合內，或投影列的 `old_ref`／`new_ref` 在集合內）查 SQL，驗 A→B→C 遮蔽前命中、遮蔽後不命中。注意 `created` 事件的初始值也算「曾經是」：要讓 `ever_in [A]` 不命中，`created` 與 A→B 兩筆事件都要遮蔽（測試兩步都驗）。M1.1 接上 Filter 後重跑同一情境。
- M1.0-D8 **`GET /issues/{key}` 與 MCP `get`**：加 `estimated_hours`（05 §3.1 審查補 R11、§4.1）；`get` 的 `fields` 可以選 `estimated_hours`。`spent_hours`、`triage`、`form_rule_marks` 等到 M1.4／M1.3／M1.5。
- M1.0-D9 **`/auth/session` 的 `capabilities.command_features`**：05 §3.10 的清單分階段長出來；M1.0 先回 `["estimated_hours"]`，`log_time`、`triage_decide`、`expected_rules_version` 在各自階段加入。
- M1.0-D10 **describe 的型別名稱**：`estimated_hours` 的 `type` 是 `decimal`，`constraints = {"min": "0", "max": "99999.99", "max_decimals": 2}`。
- M1.0-D11 **政策**：`estimated_hours` 加進 `BUILTIN_FIELDS`（委派可寫欄位）與 `delegation_field_policies` 的 CHECK；既有委派不自動加入（02 §16.3 規則 1），委派頁 `available_fields` 會列出它；新委派寫進 `writable_fields` 時預設 `direct`（05 §4.3）。
- M1.0-D12 **02 §16.7b 對應案例**：和欄位有關的 T-D1（direct／review／off）、T-D2（建立時帶欄位、政策 review）、T-D3（政策改了用同一 operation_id 重送）、T-D6（改高→改中→改回高不還原）、T-D7（direct 與送審套用都能 undo）、T-D9（遮蔽來源後前後值讀不到）用 `estimated_hours` 重跑；T-D4、T-D5、T-D8、T-D10–T-D14 和欄位種類無關，由既有測試涵蓋。
- M1.0-D13 **`issue_events_kind_check` 先加 `triage_changed`**：照 05 §2.2 的 0020；M1.3 才會寫這種事件。

## M1.0 獨立驗證後的修正（2026-11-09）

| 項目 | 處理 |
|---|---|
| D-1 `estimated_hours()` 借用 `canonical_number` 的長度上限，超長輸入回 `invalid_request` | 自己做十進位語法檢查，去掉不算數的零後才判範圍與小數位；四個案例加進單元測試與 `m1_0_estimated_hours` |
| M3 0020 回填沒在有資料時跑過 | `tests/m1_0_upgrade.rs`：暫時資料庫 `<測試 DB>_upg`，用 `db::MIGRATOR.run_to(18)` 跑 0001–0018 的原檔，灌舊資料（created、fields_changed 含 cf 與一般欄位、status_changed、duplicate_added／removed、只改 subject、一筆已遮蔽事件），再 `run` 跑 0019、0020 原檔，驗回填結果、遮蔽事件不回填、epoch 不變；用完刪掉資料庫 |
| M9 0019 的 NOT VALID 分支沒測 | 同一個升級測試先放一筆 `manage_versions` 限定 tracker 的列：升級成功、`convalidated = false`、既有列與 tracker 列保留、新的違反列仍被拒絕 |
