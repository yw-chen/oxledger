# M1 可日用：檢查清單與疑點

範圍：設計 05（`docs/design/05-m1-daily-use-contract.md`）§8 的 M1.0–M1.11。本文沒寫的共通規則沿用 05 與 02。

## 階段

| 階段 | 內容 | 完成標準 | 狀態 |
|---|---|---|---|
| M1.0 前置 | 0019、0020；`perm.rs` 權限清單；`estimated_hours` 進命令引擎、describe、事件、undo、政策；`issue_field_changes` 寫入與回填；D12 遮蔽刪投影 | 既有測試全綠；`estimated_hours` 走完 02 §16.7b 對應案例；T-M1-03 | 完成（2026-11-09，全套兩輪全綠，未 commit） |
| M1.1 Filter AST v2 | `filter.rs` 擴充成 v2（內建欄位、`me`、相對日期、`ever_in`、排序）；舊參數轉 v2；`/filter-fields`、`describe filter_fields`；`search` 收 `where` | T-M1-01…05；E4 可見性測試加 v2 隨機條件；OpenAPI 與 `api/types.ts` 重產 | 完成（2026-11-10，含獨立驗證修正；全套兩輪全綠：test-support 197 過、test-support,github 205 過；未 commit） |
| M1.2 視圖與看板 | 0021；`/views*`；內建視圖；`run`／`preview`；看板計數；專案分頁 `view`、`board`、`triage`；MCP `view_list`、`view_run` | T-M1-06…20；前端可接列表與看板 | 實作完成，含獨立驗證 9 項修正（2026-11-10；`m1_2_views` 26 個、`m1_2_upgrade` 1 個測試；全套兩輪全綠：test-support 226 過、test-support,github 234 過；前端未接；未 commit） |
| M1.3 Triage | 0022（含 `self_triage`、`same_sponsor`）；建立時進收件匣；`triage_decide`；設定；`/triage*`；Filter `triage_self_decided`；MCP `triage_list`；政策與委派頁「新操作」 | T-M1-21…30c；收件匣「最近決定」可用 | 實作完成，含獨立驗證後的修正（2026-11-10；`m1_3_triage` 6 個、`m1_3_upgrade` 1 個測試；前端未接；未 commit）。**0022 不給任何角色 `triage_issues`，M1.4 給 Manager** |
| M1.4 工時 | 0023（含 `read_own_time`）；活動與設定；`log_time`／`update_time_entry`／`delete_time_entry`；日合計與鎖順序；可見性；報表、週表；agent 限制、預設 review、`time_list` 與 A1(c) 例外；委派精靈警告；undo、D12 | T-M1-31…44、39a、39b；鎖順序壓力測試不死結；警告文案出現在委派精靈（前端截圖檢查） | 實作完成，含獨立驗證後的修正（2026-11-10；`m1_4_time` 28 個、`m1_4_upgrade` 2 個測試；警告文案只驗證 API 回應，前端未接；未 commit）。**0023 一次給內建角色 M1 權限，所有 agent 工作脈絡失效** |
| M1.5 表單與欄位管理 | 0024、0025；L1 規則進 `evaluate()`（所有入口）；L2；草稿／impact／發布／回溯；`describe_form`、`validate`、`/commands/validate`；`expected_rules_version`；自訂欄位管理與 Q-S9 轉換；conformance 向量 | T-M1-45…59 | 實作完成（2026-11-10；`m1_5_forms` 31 個、`m1_5_upgrade` 1 個測試；獨立驗證的 8 項缺陷已修、待複驗；前端未接；未 commit）。**0024、0025 不動權限、不 bump epoch** |
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

## M1.1 疑點與照建議的做法

程式：`crates/ox-core/src/filter.rs`（`Test::NotIn`、`Test::EverIn`、`eval_with`）、`crates/oxledger/src/filter.rs`（內建欄位 id −1…−15 與 `push_builtin`、`pred_prefilter`）、`crates/oxledger/src/filter/v2.rs`（`where` 解析與解析成 `Filter`、時鐘、排序、`filter_fields`）、`service.rs`（`search_in` 改寫）、`cursor.rs`（`Position::Sort`）。測試：`tests/m1_1_filter.rs`（T-M1-01…05、排序分頁、`author me`、`filter_fields`）、`tests/visibility_e4.rs`（每個世界、每個 viewer 4 個隨機 `where`，有無 `q` 各跑一次）。

- M1.1-D1 **SQL 編譯器仍只有一個**：內建欄位不再用「EXISTS 自我連接」，改成直接比欄位（`i.status_id`、`c.category`、日期欄位）；語意不變，自訂欄位維持 EXISTS＋字面 field id 的形狀。否定（`ne`、`not_in`）寫成 `IS NOT TRUE`，所以沒有值的議題也算「不等於」（05 §3.2）。`ever_in` = 目前值在集合內，或 `issue_field_changes` 的 `old_ref`／`new_ref` 在集合內；只收 status、assignee、fixed_version，自訂欄位不收（沒有投影，05 B-20）。
- M1.1-D2 **索引端仍是超集**：`index_prefilter`（版本與日期）不變；新加的 `pred_prefilter` 只把 project／tracker／assignee／非私有這幾種條件轉成 `Pred` 給 tantivy 縮小候選，其他條件一律放行，最後都以 SQL 重新檢查。E4 隨機測試有 `q` 與無 `q` 的結果一致。
- M1.1-D3 **舊參數轉成內建條件**，和 `where` AND 起來走同一個編譯器；探測（S0／S2，D14）只看 v1 `filter` 與 `where` 的自訂欄位部分，和之前一樣。舊參數的錯誤碼不變（`invalid_argument`；`version` 仍是 `invalid_filter`／`project_scope_required`），只有 `where` 回新的 `invalid_filter` 原因。
- M1.1-D4 **舊參數 `assignee=me` 也包含呼叫者所屬群組**：契約要求舊參數轉 v2，v2 的 `assignee me` 依 05 §3.2 包含群組。這是行為改變（之前只比對本人）；`assignee=<群組名>` 不變。
- M1.1-D5 **`author me`** 也比對 `created_on_behalf_of_id`（agent 代我建立的議題算「我建立的」）；`ne`／`not_in` 是兩者都不是我。
- M1.1-D6 **`overdue`**：`where` 的 `overdue eq true` = 到期日早於今天且狀態分類是 todo／doing；`eq false` 是它的補集（沒有到期日、到期日 ≥ 今天、或已完成）。舊參數 `overdue=false` 照舊不篩選。`meta.today` 現在每個回應都有（契約「一定有」），既有 `s8_versions_search` 的「沒有 overdue 就沒有 meta」改成「一定有」。
- M1.1-D7 **相對日期**：`last_n_days` = [今天−n, 今天]、`next_n_days` = [今天, 今天+n]（照 Redmine `>t-`／`<t+`，共 n+1 天）；`this_week` 依 `org_settings.week_start`；`created_at`／`updated_at` 用組織時區的日期比較（`[d 00:00, d+1 00:00)` 換成時區時間）。「今天」每個請求取一次。測試用 feature `test-support` 下的 `filter::v2::TEST_NOW` 固定時間；`fixed_version {"next": true}` 用的是 `versions` 既有的今天（真時鐘），沒接 `TEST_NOW`。
- M1.1-D8 **`invalid_filter` 的 `details.reason` 多了三種**：`unsupported_op`（欄位不收這個 op，或 op 名稱不認得）、`invalid_value`（值的型別或格式錯，例如日期、`within` 範圍、`me` 用在非人員欄位）、`invalid_shape`（不是 `{field, op, value}` 或 `{and|or: [...]}`、多了鍵）。05 附錄的錯誤表只列了 `field_unavailable`、`too_complex`、`unknown_field`、`unknown_value`、`project_scope_required`（加 R-VD9 的 `me_unavailable`），這三種是照建議補的；`details.path` 是 JSON 路徑（`where.and[1].value`）。
- M1.1-D9 **看不到 = 不存在**：專案 id 依呼叫者可見性檢查，版本 id 依所屬專案可見性；其他參照（tracker、status、priority、principal）只檢查存在。讀不到的自訂欄位（`cf:<id>` 或 `cf:<名稱>`）和不存在的欄位同一個 `unknown_field`，錯誤訊息只重複輸入。排序引用讀不到的自訂欄位也和不存在的一樣（`invalid_argument`）。
- M1.1-D10 **自訂欄位寫法**：`cf:<id>` 之外也收 `cf:<名稱>`（不分大小寫）；值可以是有型別的 `{"list": "<option id>"}`、`{"int": "12"}`…，或和 v1 一樣的單純值；list 選項必須屬於該欄位。
- M1.1-D11 **`estimated_hours`** 的值收十進位字串，也收 JSON 數字，都用 `canonical_number` 正規化。
- M1.1-D12 **`tracker`／`priority` 收 `is_null`**：照 05 §3.2 的 op 表（目前兩者都不可能是空值，結果一定是空集合）。
- M1.1-D13 **排序**：最多 3 個 key；`dir` 預設 `asc`；空值在兩個方向都排最後；最後補 `id`，方向跟最後一個 key；`key` 依專案 key 再依編號（`WEB-2` 在 `WEB-10` 前）；list 依選項 position；受限欄位用欄位 gate 包住（讀不到的議題當空值）。錯誤（未知 key、超過 3 個）回 `invalid_argument`，`details.path = "sort[n].field"`。明確指定 `updated_at desc` 與預設順序相同。
- M1.1-D14 **有 `q` 又有 `sort`**：要知道所有命中才能排序，所以候選清單一路長到 8000（既有上限），超過就照舊回 `evidence_truncated`；`rank` 仍是相關度名次。
- M1.1-D15 **cursor**：新增 `Position::Sort{k, i}`（排序值與 id）；上限從 1024 bytes 放寬到 `cursor::MAX_SEALED`（三個最長排序值的最壞情況，見下方獨立驗證後的修正第 4 項）。cursor 的擁有者與期限先檢查（別人的 cursor 一律 `invalid_cursor`，不先回參數錯誤），查詢雜湊（含正規化後的 `where` 與 `sort`）在解析參數之後比對；換了 `where` 或 `sort` → `invalid_cursor`。
- M1.1-D16 **`/filter-fields`**：`label` 用 zh-TW；`project` 選項只列看得到的專案；`assignee`／`author`／user 型自訂欄位在指定專案時 `options_source = "GET /api/v1/projects/{key}/assignees"`，沒指定專案時是 `"POST /api/v1/resolve"`；`fixed_version` 只在指定專案時有 `options_source`；指定專案時只列在該專案可能適用的自訂欄位；`restricted`（`visible = false`）與 `definition_version` 只有自訂欄位有；`triage`、`spent_hours` 不列（送了回 `unknown_field`）。專案不存在或看不到 → `not_found`。MCP `describe {"scope": "filter_fields", "project"?}` 回同一份，帶 `tracker` 或 `issue_key` 回 `invalid_argument`。
- M1.1-D17 **T-M1-03 經 Filter 重跑**：`m1_1_filter::t_m1_03_ever_in_after_redaction` 走 `where {"field": "status", "op": "ever_in"}`，遮蔽 A→B 後仍因 `created` 命中、兩筆都遮蔽後不命中、Doing 仍由 Doing→Done 命中；有 `q` 時結果相同。

## M1.1 獨立驗證後的修正（2026-11-10）

| 項目 | 處理 |
|---|---|
| 1（中）`in` 的每個值逐一 `resolve_one`、不去重：30 條件 × 100 個同名狀態一個請求 15 秒 | 每個請求記住已解析的值（同一個名稱或 id 只查一次）；內建參照欄位的 id 先一次批次查（每種一個查詢）；不同名稱的查詢每個 `where` 最多 `MAX_NAME_LOOKUPS = 100` 個，超過回 `invalid_filter`／`too_complex`（id 不算）。測試 `m1_1_value_lookups_are_bounded`：30×100 個同名、3×100 個同 id、3×100 個不同 id 都 < 1 秒；100 個不同名稱可用，101 個回 `too_complex` |
| 2（低）`"-5000-01-01"` 等 chrono 收、PG 超範圍 → 500 | 日期限年份 1–9999，否則 `invalid_value`（`m1_1_dates_out_of_range_and_unknown_version_names`，含 `created_at`） |
| 3（低）`fixed_version` 名稱不存在時 details 沒有 reason | `remap` 把沒有 reason 的 `invalid_filter` 也當成 `unknown_value`（同一測試） |
| 4（中低）4096 bytes 上限收不回伺服器自己發的排序 cursor | 上限改成 `cursor::MAX_SEALED` = 24 + 16 + 1024 + 3 × 6 × 2000（三個 2000 bytes 字串值、每 byte 跳脫成 `\u00XX` 的最壞情況），`encode` 超過就報內部錯誤而不是發出收不回的 cursor；測試 `m1_1_cursor_with_the_longest_sort_values` 用 1999 個控制字元或 `"` 的值、1～3 個排序鍵逐頁翻完 |
| 5 測試缺口 M4：`cf_cond` 的 gate 拿掉沒被抓到 | `m1_1_custom_field_gates_in_where_and_sort`：欄位改成只適用 WEB 後 AUTH 的殘留值、只有 Lead 角色可讀的欄位在 AUTH 的值，`eq`／`in`／`not_null` 都篩不出（有無 `q`）；把 gate 換成 `Pred::True` 時測試失敗（已實測） |
| 6 測試缺口 M9：cf 排序的 gate 拿掉沒被抓到 | 同一測試：讀不到／不適用的值當空值排最後、依 id；拿掉排序 gate 時測試失敗（已實測） |
| 7 偶發 `custom_fields_pg::custom_field_filters_match_reference_on_pg` | 建索引前把 Acme 議題的 subject 加長，使其 BM25 分數嚴格最低，前提「有命中排在前 500 名外」不再依賴 segment 切法 |

## M1.2 疑點與照建議的做法

- M1.2-D1 **`run` 與 `preview` 走 POST**（契約本來就是 POST）：cursor 放在 body，不經 query string。排序 cursor 最長約 37 KB（`cursor::MAX_SEALED`），base64 後約 50 KB，GET 的 URL 長度放不下。`m1_2_http_run_with_the_longest_cursor` 經 HTTP 用三個 2000 字元的排序值加分組逐頁翻完，cursor 長度超過 36000 字元仍收得回。分組與泳道的前導排序欄只用 id／position（不用名稱），所以 cursor 仍在 `MAX_SEALED` 的 1 KB 餘裕內。
- M1.2-D2 **`manage_public_views` 不在 0021 給任何角色**：0021 不動 `role_permissions`，所以不會 bump epoch（同 0019 的處理）。開發用 seed 給 Manager。使用者決定（2026-11-10）：M1 新權限照 Redmine 預設給內建角色，**在 M1.4 的 migration 一次給完**（Manager 拿到 `manage_public_views`），只 bump 一次 epoch。
- M1.2-D3 **專案視圖與綁專案的個人視圖**：存檔時 `where` 頂層若沒有 `project eq <該專案>`，伺服器就補上（補了超過深度或條件數上限時不補）；執行時一律再 AND 該專案（R-T3）。條件中的 `project` 只能是該專案，否則回 `invalid_view`／`project_scope`。專案視圖不可用受限欄位（`where`、欄位、卡片欄位、排序、分組、分欄都檢查），回 `invalid_view`／`restricted_field`。
- M1.2-D4 **欄位鍵**：列表欄位與卡片欄位可用 `key、subject、project、tracker、status、status_category、assignee、author、priority、fixed_version、is_private、start_date、due_date、created_at、updated_at、estimated_hours`，以及 `cf:<id>`（也收 `cf:<名稱>`，存成 id）。`group_by` 可用 `status、assignee、priority、tracker、fixed_version、project`，以及 list、user、bool 型自訂欄位。`lanes.by` 可用 `status、status_category、assignee、priority、fixed_version`，以及 list、user 型自訂欄位。`swimlanes.by` 可用 `assignee、priority、fixed_version、project`。`totals` 只有 `estimated_hours`。`spent_hours`、`triage` 這階段不開放，送了回 `invalid_view`／`unknown_field`（`where` 裡則是 `invalid_filter`／`unknown_field`）。
- M1.2-D5 **預設值**：列表欄位 `key、subject、status、assignee、priority、updated_at`；卡片欄位 `key、subject、assignee、priority、due_date`；`done_window_days` 14（範圍 1–365）；`show_empty` true。上限：欄位最多 12 個、卡片欄位 8 個、欄 30 個、每欄的值 100 個、欄標題 30 字、`q` 500 字、定義 32 KB。欄沒有 id 就自動補 `l1`、`l2`…。
- M1.2-D6 **看板「完成」欄的時間窗**：不是 done 的單一律顯示；done 的單只顯示 `updated_at` 落在最近 `done_window_days` 天內的（用 `last_n_days`，以組織時區換日）。計數也套同一條件。
- M1.2-D7 **沒有列出 `lanes.items` 時自動產生的欄**：
  - `status_category`：三欄（待處理、進行中、完成）。
  - `priority`：所有啟用中的優先度，沒有「未設定」欄（priority 不會是空值）。
  - `fixed_version`：該專案未關閉的版本，再加「未設定」。沒有綁專案時必須列出欄，否則回 `lanes_required`。
  - `assignee` 與 user 型自訂欄位：可見結果中實際出現的人（依名稱排序），再加「未指派」／「未設定」。
  - list 型自訂欄位：啟用中的選項，再加「未設定」。
  - 人員欄是一個查詢（套用可見性、視圖條件與「完成」時間窗）取出的，不用計數預算，計數逾時也不會讓欄消失。
  - 自動欄最多 30 欄（含空值欄）；值超過時只取前面的，`unmapped.lanes_truncated` 是 true。
  - 沒有欄的值（停用的選項、已關閉的版本、超過上限的人、沒對到的狀態）以及自訂欄位不適用的單，都算進 `unmapped.issues`，不會無聲消失。
  - `status`：一定要列出欄，否則回 `lanes_required`。
  - 自動欄的 id 是值本身（uuid 或分類名），空值欄的 id 是 `none`。
- M1.2-D8 **`unmapped`**（每個看板都有）：
  - `status_count`：只在 `lanes.by = status` 時有，是全站沒對到任何欄的狀態數（狀態是全域的，不分專案）。
  - `issues`：可見、符合視圖條件、「完成」時間窗與泳道篩選，但不在任何欄的單數。計數規則同 D9（999 上限、逾時或有 `q` 時是 `null`）。
  - `lanes_truncated`：自動欄因 30 欄上限少列了值。
  - agent 的 `view_run`（board）也回 `unmapped`。
- M1.2-D9 **計數與合計**（R-VD6）：
  - 對呼叫者看得到的單計算，最多 999；超過時回 `{"value": 999, "capped": true}`（查詢時用 LIMIT 1000）。
  - 一次執行的所有計數與合計共用 2 秒預算，從第一個計數開始算；列表與各欄的項目查詢先跑完，不吃這個預算。每個查詢放在 savepoint 裡並設 `statement_timeout` 為剩餘時間；逾時就回 `null`（合計回 `complete: false`），已取得的列表項目照常回。
  - `wip.exceeded` 由計數算出；計數是 `null` 時也是 `null`。
  - `estimated_hours` 合計以兩位小數字串回傳，例如 `"18.50"`。
  - 有全文 `q`（視圖的 `query.q` 或 `override.q`）時不算計數與合計，都回 `null`：全文搜尋最多只看 8000 筆，算出的數字不準。
  - 測試掛勾：feature `test-support` 下的 `views::COUNT_DELAY_MS` 與 `COUNT_BUDGET_MS`。
- M1.2-D10 **`override.q` 取代視圖自己的 `query.q`**，不是兩者 AND；`override.where_and` 與視圖條件 AND。cursor 綁定主體、視圖 id、`def` 的 hash、`where_and`、模式、lane、swimlane；`q` 本來就在搜尋的 cursor hash 裡。視圖被改後拿舊 cursor 回 `invalid_cursor`（T-M1-15）。看板的 cursor 必須同時帶 `lane`，否則回 `invalid_argument`。
- M1.2-D11 **執行時讀不到的自訂欄位**（R-VD4）：用在 `where`、排序、分組、分欄、泳道 → `invalid_filter`／`field_unavailable`（`details.path` 指向 `def.…`）。只用在列表欄位或卡片欄位 → 直接拿掉該欄，照常執行。
- M1.2-D12 **內建視圖**：
  - `builtin:assigned_to_me`：指派給我（含我的群組）且狀態未完成，依到期日升冪、再依 priority 降冪。
  - `builtin:reported_by_me`：我建立的（含 agent 代我建立的）且狀態未完成，依更新時間降冪。
  - `builtin:involving_me`：指派給我、我建立的，或我在 30 天內寫過事件（`issue_events.actor_id` 或 `on_behalf_of_id` 是我）。最後這個條件不是 Filter AST 能表達的欄位，所以用內部欄位 `filter::WROTE_RECENTLY_FIELD` 在執行時 OR 上去。因此這個視圖不能複製，複製時回 `forbidden_op`／`not_copyable`。
  - `builtin:project_issues:<project>`：該專案未完成的單。
  - `builtin:project_board:<project>`：依狀態分類分三欄。
  - 專案類內建視圖的 `<project>` 也收專案 key。
  - 所有內建視圖都不能改，改了回 `forbidden_op`／`builtin_view`；可以複製。
- M1.2-D13 **複製**：預設複製成個人視圖，名稱加「（副本）」；也可以指定 `scope: project`（需要 `manage_public_views`）。複製到別的專案時，會先拿掉來源的 `project eq` 條件，再由伺服器補上新專案的。事件 payload 記 `copied_from`。冪等：先查 Idempotency-Key 再讀來源，fingerprint 只由請求（來源 id、scope、project_id、name）算，所以來源改名或刪除後同 key 同內容重送，仍回當初建立的副本（201）。
- M1.2-D14 **刪除與分頁**：
  - 鎖的順序：project `FOR SHARE` → `project_tab_settings` `FOR SHARE` → view `FOR UPDATE`。分頁寫入是 project `FOR SHARE` → settings `FOR UPDATE` → view `FOR SHARE`，兩邊順序一致。
  - 被分頁引用的視圖不能刪，回 `view_in_use`，`details.tabs` 是那些分頁的標題。
  - 個人視圖不會被分頁引用，刪除不受影響。
  - 刪除是軟刪除，`lock_version` 加 1。
- M1.2-D15 **專案分頁**：
  - 新分頁種類 `view`（id 前綴 `v_`），算進 10 個自訂分頁的上限。
  - 內建 `board` 可帶選填的 `view_id`，必須指向該專案的 board 專案視圖。其他內建分頁帶 `view_id` 回 `not_allowed`。
  - `view_id` 不是該專案、未刪除的專案視圖時回 `invalid_value`／`tab_view_invalid`。
  - `triage` 分頁預設隱藏，可以先開；Triage 的內容到 M1.3 才有。
  - `GET /projects/{id}/tabs/{tab}/issues` 仍只跑 query 分頁；view 分頁由前端呼叫 `POST /views/{view_id}/run`。
- M1.2-D16 **建立回 201**：`POST /views` 照契約回 201。`POST /views/{id}/copy` 也是建立，同樣回 201；同一個 Idempotency-Key 重送也回 201。OpenAPI 用 `openapi::CREATED_ROUTES` 標示這兩條路由，`types.ts` 從 201 的回應取型別。其他端點維持 200。
- M1.2-D17 **`meta.today`** 放在 `data.meta.today`，與 `search` 的 `SearchMeta` 相同；`meta` 封套仍是 `request_id` 與 `observed_at`。契約範例把 `today` 放在封套的 `meta`，這裡沿用 M1.1 搜尋已採用的位置（使用者 2026-11-10 確認維持）。
- M1.2-D18 **事件與遮蔽**：
  - `view_def_events` 記 `created`、`updated`、`deleted`，payload 是 `{before, after}`。只改到相同值時不記事件，`lock_version` 也不變。
  - 遮蔽表（`view_def_event_redactions`、`view_defs.redacted_at`）已經建好，但遮蔽端點這階段不做（D12 的視圖遮蔽列為未完成）。
  - 讀事件時，有 `redaction_id` 的事件 payload 回 `"[已遮蔽]"`。
- M1.2-D19 **agent**：
  - `view_list` 只列委派專案的專案視圖，以及每個委派專案的 `project_issues`、`project_board`。不列個人視圖，也不列含 `me` 的內建視圖；`editable` 一律是 false。
  - `view_run` 用 agent 自己的讀取範圍執行。list 回 `search` 的 compact 列；board 回 `lanes[{id, title, count, items, next_cursor}]`。
  - 視圖含 `me` 時回 `invalid_filter`／`me_unavailable`（R-VD9）。
  - 個人視圖或未委派專案的視圖回 `not_found`。
  - REST 端點都要瀏覽器 session。
- M1.2-D20 **上限**（R-V4）：每人最多 100 個個人視圖，每個專案最多 50 個專案視圖；用 advisory lock 計數，超過回 `invalid_value`／`view_limit`。名稱 1–60 字，NFC 正規化後 casefold 比對；同一範圍內重複回 `view_name_taken`（`details.view_id`）。並行時（另一個交易剛建立或改成同名）由唯一索引擋下：建立與複製用 `ON CONFLICT DO NOTHING`，改名在 savepoint 裡更新、接住 23505，三者都回 `view_name_taken`，不會 500。
- M1.2-D21 **分組與分欄的排序**：伺服器把分組或泳道的值放在 `sort` 前面當前導排序欄，所以同一組的項目會連在一起、可以跨頁。順序：
  - status、tracker、priority：依 position。
  - 版本：依 `effective_date`。
  - 人員：依 id。這不是名稱順序（使用者 2026-11-10 確認維持；前端可自行依名稱顯示）。
  - 專案：依 key。
  - list 型自訂欄位：依選項 position。
  - 空值排最後。
  - 自訂欄位先依「適用與否」排：適用的在前；不適用的單（tracker、專案不符或讀不到）排在最後，分組值是 `"not_applicable"`、標籤「不適用」，和「未設定」（適用但沒填）分開。`groups[].count` 用同一個定義（不適用 = 欄位的 gate 不成立），所以各組計數加起來等於可見結果數。泳道也會出現「不適用」，但不能用 `swimlane=not_applicable` 單獨開啟（回 `invalid_argument`）。
  - 回應的 `groups` 與 `swimlanes` 只列本頁出現的值。`groups[].count` 則是該組全部可見單的數量。
- M1.2-D22 **綁專案的個人視圖，專案看不到了**（例如被移出私有專案）：`GET /views` 與 `GET /views/{id}` 不回 `project`（不洩漏名稱），改回 `project_unavailable: true`；執行回 `invalid_view`／`project_unavailable`（`details.path` 是 `project_id`）。擁有者仍可以讀、改名或刪除它。
- M1.2-D23 **獨立驗證後補的測試**（`m1_2_views.rs`）：user 型自訂欄位自動分欄（含 HTTP，不再 500）；不適用的分組、計數與看板 `unmapped`；人員欄不受計數逾時影響、超過 29 人的歸屬；停用選項進 `unmapped`；並行改名回 `view_name_taken`；來源改名、刪除後複製重放；移出專案後的個人視圖；DELETE 的 `lock_version`；`involving_me` 的 30 天邊界（29、31 天）；執行時一律 AND 綁定專案（存檔定義沒有 `project` 條件時）；agent 不能當視圖擁有者（DB trigger，INSERT 與 UPDATE）。已關閉版本進 `unmapped` 走的是和停用選項同一段程式，沒有單獨測；「計數預算從第一個計數開始」沒有計時測試。

## M1.3 疑點與照建議的做法

- M1.3-D1 **`triage_issues` 不在 0022 給任何角色**（使用者決定，2026-11-10）：0022 不動 `role_permissions`，不 bump epoch。**M1.4 的 migration 給 Manager**（和 `manage_public_views`、工時權限一起，一次 bump）。在那之前要用 Triage，由管理員手動給角色。
- M1.3-D2 **R25 `issues_id_project_uq` 放在 0022**：`issue_triage` 用 `(issue_id, project_id)` 複合外鍵（`ON UPDATE CASCADE`）指向它；0023 不要再加。
- M1.3-D3 **`duplicate` 決定要兩個委派操作**：agent 除了 `triage_decide`，委派也要有 `duplicate_of`；政策取兩者較嚴的（`policy_items` 兩個都列）。理由：duplicate 決定會建立重複關係，和直接送 `duplicate_of` 是同一件事，不該因為包在 triage 裡就繞過 `duplicate_of` 的政策。**待使用者確認，現行為預設。**
- M1.3-D4 **文件結構（T-M1-23）**：一份文件最多一個 `triage_decide`；其他 op 都要是同一張單。可搭配的 op：`accept` 可帶 `update_fields`、`transition`、`add_note`；`decline`、`duplicate` 可帶 `transition`、`add_note`；`duplicate` 一定要有 `transition`（`decline` 見 D5）；`snooze`、`unsnooze` 只能帶 `add_note`。不能另放 `duplicate_of` op（`duplicate_of` 寫在 `triage_decide` 裡）。違反的都回 `invalid_request`。
- M1.3-D5 **R27**：`decline`／`duplicate` 結束時單的狀態不是 done 類，回 `invalid_value`（`kind: "triage_requires_close"`、`path: "ops"`），因為要查資料庫。`duplicate` 完全沒帶 `transition` 是結構錯 `invalid_request`。
  - **獨立驗證後（2a）**：pending 的單已被一般 `transition` 關成 done 類時，`decline` 可以不帶 `transition`，決定照常記錄；單還沒關時不帶 `transition` 的 `decline` 回 `invalid_value`／`triage_requires_close`（不是 T-M1-23 字面上的 `invalid_request`，理由同 R27）。收件匣照常列這種已關閉的 pending 單，直到被決定。
- M1.3-D6 **snooze 範圍**：`snooze_until` 要在 1 小時到 90 天之間（下限有 1 分鐘寬限：≥ 59 分鐘即可，client 算的「現在＋1 小時」不會被拒），否則 `invalid_value`（`kind: "snooze_range"`）。`unsnooze` 一張沒有生效中 snooze 的單（沒 snooze、已到期、或之後有人改過單），回 `triage_state_invalid`（`details.state`、`snoozed: false`）。
- M1.3-D7 **新錯誤碼 `triage_state_invalid`**（409，`next_actions` 是 refresh_and_revise、stop）：單不在 Triage 裡（`state: "none"`）或已經決定過（`state` 是現在的狀態）。
- M1.3-D8 **審查佇列**：`accept`（不帶 note）與 `snooze` 可以勾選批准；`decline`、`duplicate`、`unsnooze` 有 `triage:<decision>` 原因，要開來看。審查 diff 多一列「Triage」（before 是現在的狀態，after 是 `{decision, snooze_until?, duplicate_of?}`）。
- M1.3-D9 **人批准 agent 的提案**：決定者（`decided_by`）記成提出提案的 agent，A5 標記照 agent 算。**待使用者確認，現行為預設**（另一個做法：記成批准的人、標記 false）。
- M1.3-D10 **設定變更歷史**：寫進 `project_setting_events`（新 kind `triage_settings_updated`，CHECK 用聯集重建）。
- M1.3-D11 **設定權限**：`manage_project` 的人可改；agent 一律 `can_manage: false`、PUT 回 `forbidden_op`。回應帶 `warnings`：有 `triage_issues` 但 workflow 沒有轉到拒絕狀態的角色（`decline_transition_missing`，不擋）。驗證錯誤都是 `invalid_value`，`kind` 為 `triage_entry_rule`、`triage_decline_status_required`（啟用時要有拒絕狀態）、`triage_decline_status_invalid`（不是 done 類）、`triage_responsible_invalid`（不是這個專案可指派的人）。PUT 收 `Idempotency-Key`、`lock_version` 不符回 `conflict`（附 `current`、`current_lock_version`）。
  - **獨立驗證後（1）**：同一個 key、同樣內容重送，回的是**原本那次寫入的回應**（當時的 `lock_version` 與 `warnings`，存在 `operation_requests.result_ref.settings`），不是現在的設定。`project_tabs.rs` 的 PUT 有同樣的模式（重送回現在的分頁設定），不在本階段範圍，列為後續項。
  - **獨立驗證後（2c）**：只驗證這次有改的欄位。既有但已失效的負責人（不能再被指派）或拒絕狀態（不再是 done 類）不擋存檔（例如關閉 Triage），改由 `warnings` 提示：`responsible_invalid`、`decline_status_invalid`；有改到該欄位才擋。0022 的 check trigger 也只在該欄位新設或變更時檢查。`GET` 也回 `warnings`（只給 `can_manage` 的人）。
- M1.3-D12 **進收件匣的規則**：專案 Triage 啟用時，agent 建的單一律進；`all_new` 全部進；`creator_without_triage`（預設）是建立者沒有 `triage_issues` 才進。`created` 事件 payload 多一個頂層 `"triage": "pending"`。`describe` 的 `creation.enters_triage` 照呼叫者算。Triage 關閉時建的單不進（之後再開也不補）。
- M1.3-D13 **Triage 關閉後**：收件匣（`/projects/{id}/triage`、`/triage`、`/triage/counts`）不列關閉專案；已在 pending 的列保留，重新啟用就回來。`triage_decide` 在關閉時仍可送（不擋）。「最近決定」不看啟用與否。
- M1.3-D14 **Filter `triage` 的值**：`pending`（不含生效中的 snooze）、`snoozed`、`accepted`、`declined`、`duplicate`、`none`（沒進過 Triage）。關閉專案的 pending 列在 filter 裡仍是 `pending`。`triage_self_decided` 是 `self_triage OR same_sponsor`。兩者都可用在視圖的 `where`；視圖欄位多了 `triage`（M1.2-D4 說的「這階段不開放」到此解除；`spent_hours` 仍未開）。
- M1.3-D15 **`same_sponsor`**：決定者是 agent、作者是另一個 agent，兩者的 `agent_principals.delegator_user_id` 相同。同一個 agent client、同一個人的 agent principal 是同一個，所以舊委派與新委派的同一個 agent 建單後自己決定，算 `self_triage`。
- M1.3-D16 **計數**（R28）：`/triage/counts` 用 `/triage` 的同一組專案（看得到、啟用、有 `triage_issues`；agent 再加委派），每專案 `count` 最多算到 99，超過時 `more: true`。只算呼叫者看得到的單。
- M1.3-D17 **`GET /triage` 不收 `view=decided`**：回 `invalid_argument`；「最近決定」只在專案層（`/projects/{id}/triage?view=decided`），近 30 天，`self_triage=only|exclude|all`、`decided_by_kind=agent|user|all`，依 `decided_at` 由新到舊。
- M1.3-D18 **undo**：決定記成 `_triage` 假欄位（整列快照），undo 放回決定前（pending、A5 標記清掉），決定本身的 `triage_changed` 事件保留原標記；之後有人改過（快照不同或來源不是這次執行）回 `changed_since`；現在沒有 `triage_issues` 回 `permission`。`UndoInput.skip_triage` 可以略過個別單；`UndoResult.triage_restored` 回放回幾張。undo 本身可以再 undo。
  - **獨立驗證後（2d）**：undo 一個 `decline`／`duplicate` 決定，Triage 狀態回 pending、單回到收件匣，但單的狀態仍是 done 類（照契約，`transition` 是另一項，要在 undo 預覽裡另外選才會還原；`skip` 掉或不能還原時就停在 done）。分流者在收件匣看到的是一張已關閉的 pending 單，可以直接 `decline` 不帶 `transition`（見 D5）。
- M1.3-D19 **agent 看不到私有單**（§3.5 既有規則）：所以 agent 也不能分流私有單，`triage_list` 不列私有單。
- M1.3-D20 **既有委派**：0022 不改 `delegations.proposal_ops`、不加 `delegation_command_policies`；委派頁的 `available_commands` 會出現 `triage_decide`，委派人自己加。新委派照 §4.3 預設 direct。

### 測試

- `m1_3_triage.rs`：
  - `m1_3_triage_people`：設定（權限、驗證、警告、冪等、衝突）、T-M1-21…27、T-M1-30、R28 計數、分頁 cursor、Filter `triage`、視圖欄位、單張詳情 `triage`、`describe` 的 `enters_triage`、審查佇列形狀。
  - `m1_3_triage_agents`：T-M1-28（既有委派、direct、review、off、duplicate 要 `duplicate_of` 政策）、T-M1-29、T-M1-30a…30c、E4w（沒委派的專案、看不到的專案、委派人沒有 `triage_issues`）、MCP `triage_list`。
  - `m1_3_constraints`：CHECK 清單與程式常數一致、trigger、複合外鍵。
  - 獨立驗證後補：
    - `m1_3_settings_replay_and_stale`：重送回原回應、換內容回 `idempotency_key_reused`、失效值變警告不擋、有改才擋。
    - `m1_3_closed_pending_snooze_slack_counts`：已關閉的 pending 單（收件匣、`decline` 不帶 `transition`）、snooze 寬限、`issues_visibility=default` 的分流者計數不含看不到的私有單。
    - `m1_3_http`：經 router 的 `/triage*`、`/triage-settings`（401、403、404、缺 Idempotency-Key、重送、`conflict`、未知欄位、cursor 翻頁、壞 cursor、`invalid_argument`、單張詳情）。
    - `people` 加：人分流自己建的單標記為 false。
    - `agents` 加：委派沒有 `triage_decide` 的 agent `triage_list`（不帶 project）是空的；人批准 agent 的 triage 提案（實際套用、A5 標記、`decided_by` 是 agent）；undo 的 undo 實際套用、之後再 undo 原決定是 `changed_since`（狀態相同但來源不同）、`skip_triage`。
    - 手動 mutation 確認：拿掉 `undo.rs` 的 `from_here`、拿掉 `triage_projects` 的委派判斷，測試都會失敗。
- `m1_3_upgrade.rs`：0021 → 0022。
- 沒測到：MCP `triage_list` 只測了 `agent_list` 函式，沒經 MCP 協定。
- 後續項：`project_tabs.rs` PUT 分頁設定的 Idempotency-Key 重送回現在的設定（同 D11 修正前的模式）。

## M1.4 疑點與照建議的做法

- M1.4-D1 **內建角色的 M1 權限**（使用者決定，2026-11-10）：0023 用一個 `INSERT` 照 Redmine 預設給內建角色，所有專案的 `security_epoch` 只加一次。
  - Manager：7 個全給（`view_time_entries`、`log_time`、`edit_time_entries`、`edit_own_time_entries`、`log_time_for_other_users`、`triage_issues`、`manage_public_views`）。
  - Developer、Reporter：`view_time_entries`、`log_time`。
  - **Non member／Anonymous 不給 `view_time_entries`**（Redmine 預設有）。理由：公開專案的非成員會看到全部人的工時（`time_entries_visibility` 預設 `all`），會影響產品與隱私，沒有合理預設。**待使用者決定**；要給時由管理員手動加（會再 bump 一次 epoch）。
  - 角色照 0015 的做法用名稱找（`builtin IS NULL`）；改過名的角色拿不到，要手動給。
- M1.4-D2 **既有專案一律啟用 `time_tracking` 模組**（Redmine 預設模組含它）。這一個 `INSERT` 期間停用 `enabled_modules` 的 epoch trigger，所以每個專案的 epoch 只因為上面的授權加一（升級測試驗證「恰好 +1」）。
- M1.4-D3 **回執**：契約 §3.10.2 寫的是 `effects` 加 `{kind: "time_entry", …}`；實作放在獨立的 `time_entries` 陣列（`/commands` 的 `CommandResult.time_entries`、`command_submit` 的 `Applied.time_entries`；空的時候省略），每項 `{kind: "time_entry", id, lock_version, changed, action}`。理由：`effects` 的既有元素是 issue 形狀（`key`、`search` 等），前端與 agent 都以 issue 解讀，混進另一種形狀會壞既有 client。items 另有 `time_entry_id`。
- M1.4-D4 **agent 回執只給 id 與 `lock_version`**，不論 `read_own_time` 開不開（§3.5.4 只要求關的時候不給欄位；開的時候也用 `time_list` 讀，行為單純）。
- M1.4-D5 **預設值**：`spent_on` 省略時用組織時區的今天；`activity_id` 省略時用專案可用的預設活動，沒有預設時回 `required_field_missing`（`fields: ["activity_id"]`）。`user_id` 省略是自己（agent 是委派者）。
- M1.4-D6 **專案工時設定版本**：新表 `project_time_settings`（每專案一列，trigger 建列）放 `lock_version`，給 `PUT /projects/{id}/time-activities` 做樂觀鎖；同一個 PUT 可帶 `module_enabled` 開關模組（要 `manage_project` 或管理員）。
- M1.4-D7 **組織規則預設**：`timelog_max_hours_per_day = 0`（不限）、可記 0 小時、可記未來日期、可記已關閉的單、不要求 issue、不要求說明（同 Redmine 預設）。
- M1.4-D8 **配額**：`log_time` 每個 agent 每小時 20 次，只算 direct 寫入（送審的提案不算，核准時也不算），和既有配額同一張表。
- M1.4-D9 **委派的 `read_own_time`**：
  - API 建委派時預設 `false`；要開必須同時帶 `read_own_time_ack: true`，否則 `invalid_value`（`kind: "ack_required"`）。DB 也有 CHECK（開著就要有 `read_own_time_ack_at`）。
  - 另開 `PUT /delegations/{id}/read-own-time`（`{enabled, ack}`）切換。**開或關都 `policy_version + 1`，並要求 step-up**（契約 §2.5：委派設定變更；比照 `PUT /delegations/{id}/policies`）。原本「不 bump」的做法經獨立驗證撤回。
  - 委派回應帶 `read_own_time_warning`：§6.14 文案逐字（`title`、`body`、`ack_label`、`tag`、`tag_tooltip`），前端直接顯示。
- M1.4-D10 **受眾檢查**（§3.8）：工時目的地的讀者＝工時讀者（`time_visibility`）∩ issue 讀者；私有 issue 上的工時略過；不一致時錯誤 `kind` 用 `private_destination`。
- M1.4-D11 **時數格式**：回應一律兩位小數字串（`"1.50"`）；0 寫成 `"0"`。報表與視圖合計是 `numeric(12,2)` 文字（`"0.00"`）。
- M1.4-D12 **封存的 issue 上的工時看不到**（清單、報表、合計都不算），和 issue 本身一致。
- M1.4-D13 **REST 讀取端點只給人**（`/time-entries*`、`/me/timesheet`、`/time-activities` 等）；agent 只能用 MCP `time_list`。
- M1.4-D14 **`spent_hours`**（§3.5.4、R-VD7）：
  - issue 詳情（`GET /issues/{key}` 與 MCP `get` 不帶 `fields`）：人看到自己看得到的工時合計；在該專案讀不到工時時**省略欄位**；agent 永遠沒有。
  - `get` 的 `fields` 可要 `spent_hours`（人：讀不到時是 `null`）；agent 要它回 `invalid_argument`（`details.reason = "unavailable_to_agent"`，T-M1-40）。
  - **Filter 欄位 `spent_hours`**（number；`eq`、`ne`、`lt`、`le`、`gt`、`ge`、`between`；值是小數字串）：比較的是觀看者看得到的合計，讀不到工時的專案是 NULL（`ne` 會符合，同其他欄位的「沒有值算不相等」）。契約 §3.2 的內建欄位表沒有列 `spent_hours`，這是依 M1.4 任務說明加的；agent 用它回 `invalid_argument`（`unavailable_to_agent`），`/filter-fields` 對 agent 不列。**不開放排序**（契約沒寫，成本高）。
  - 實作：`ox_core::filter::Filter::Cond` 多一個明確欄位 `viewer: Option<Uuid>`（「這個值是誰看到的」；只有 `spent_hours` 用，`Filter::cond_as` 建立），`applies` 維持 `True`，和其他內建欄位一樣。原本把觀看者塞在 `applies = Pred::AuthorIs(viewer)` 的做法已拿掉（會被當成 gate，例如 views 的 `Clause::NotApplies`）。prefilter 不認得這個欄位，一律不收窄。
  - 視圖：欄位 `spent_hours`（人：看得到的合計字串，讀不到是 `null`）與合計 `totals: ["spent_hours"]`（整個可見結果，R-VD6 同一個 2 秒預算）。agent 的 `view_run` 不回欄位值與合計，所以不受影響。M1.2 的 `LATER` 清單移除。
- M1.4-D15 **undo**（T-M1-41）：只處理 agent 執行的工時變更（人的執行不留前後值）。
  - 建立的工時：沒人改過（所有欄位來源仍是這次執行、沒被刪）→ 軟刪除；否則 `changed_since`。
  - agent 改過的欄位：來源仍是這次執行 → 直接放回前值（不重跑 T1–T7 驗證，例如日上限；前值當時是合法的）；`issue_id`、`user_id` 回 `not_supported`（agent 不能改 `user_id`；改 issue 要人手動），`comments` 的前後值被遮蔽時回 `redacted`。
  - 權限：改欄位的還原要能改那筆（`edit_time_entries`，或自己的工時有 `edit_own_time_entries`），否則 `permission`。**agent「建立」的工時（`created`：`user_id` 是 undo 的人、之後沒人改過）只要 `log_time` 就能撤回**（照預設，**待使用者確認**）。理由：Developer 預設只有 `view_time_entries`、`log_time`，否則 Developer 委派者收不回 agent 替他記的工時。Redmine `lib/redmine/default_data/loader.rb` 的 Developer 預設權限：repo 與本機都沒有 Redmine 原始碼，**沒有查證**；依記憶 Developer 有 `log_time`、`view_time_entries`，沒有 `edit_own_time_entries`，要使用者或下一輪對照原始碼確認。
  - 只把實際寫回的欄位標 `reverted_by`（建立的標 `_created`）；用 `skip_time_entries` 跳過的欄位之後仍可 undo。
  - 鎖順序：projects FOR SHARE → issues（含工時所在的 issue）FOR UPDATE → time_entries FOR UPDATE → 日合計 → 執行紀錄。`UndoInput.skip_time_entries`（`"<id>/<field>"`，新建的是 `created`）、`UndoResult.time_entries_restored`、`UndoPreview.time_entries`。
- M1.4-D16 **D12 遮蔽工時說明**（T-M1-42）：新端點 `POST /time-entries/{id}/redact`（同其他遮蔽：`manage_project` 或管理員、step-up、Idempotency-Key、epoch +1）。
  - `time_entries.comments` 換成 `[已遮蔽]`、`redacted_at`；每個含 comments 的事件只換掉 comments 的值（時數等其他變更保留），各寫一筆 `time_entry_event_redactions`；回應的 `redaction_id` 是第一筆。
  - 衍生副本：`redaction::derived` 加上 `execution_time_entry_changes`（該專案的工時，或來源快照含該專案的執行），整個前後值換成標記；所以專案內任何一次遮蔽之後，agent 工時變更的 undo 會變成 `redacted`（建立的工時仍可刪），和 issue 欄位的行為一致。
  - 讀不到那筆工時的人（非管理員）回 `not_found`。已軟刪除的工時也可遮蔽（同權限）。
  - 遮蔽後再改 comments 會清掉 `redacted_at`（新文字不是遮蔽過的內容），所以可以再遮蔽一次（`changed: true`）。undo 寫回 comments 時同樣處理。
- M1.4-D17 **待審佇列**（§4.3、C4）：`log_time` 沒有 comments 或 comments ≤ 80 字可勾選，超過是 `long_field:comments`；`update_time_entry` 一律 `op:update_time_entry`（逐份看）。diff 多一列 `field: "time"`（`after: {hours, spent_on, activity: {id, name}, comments, user_id}`）；沒有 issue 的 `log_time` 沒有 diff 列（`DiffTarget` 需要 issue）。
- M1.4-D18 **MCP**：新工具 `time_list`（`{project?, issue?, from, to, limit?, cursor?}`，描述照 §4.2 逐字）；`command_submit` 描述加「log_time 只能替委派者記、agent 不能刪工時」兩句；server instructions 加「工時只能代委派者記、說明不要抄進 note」。§4.2 的 `agent_chain` 一句與 instructions 的 `describe_form`／`validate` 一句**沒加**：那是 M1.9、M1.5 的功能，現在加會讓 agent 找不存在的工具。

- M1.4-D19 **鎖順序（§3.5.5）的實作細節**：
  - 日合計一列一列鎖（`(user, day)` 排序後逐筆 `SELECT … FOR UPDATE`），鎖完第一列有測試用 fault point `time.after_first_total`（只在 `test-support`）。
  - 混合文件（`log_time` 加同一張 issue 的 op）：引擎在鎖完 issue、版本之後，field option 與 search token 之前，先呼叫 `time_entries::prelock_totals` 鎖文件可能動到的日合計（`spent_on` 無法解析的不鎖，之後 plan 也會拒絕）；plan 之後再鎖同一批列是 no-op。
- M1.4-D20 **update／delete 的錯誤順序**：看不到的工時一律 `not_found`，在專案、模組、權限檢查之前判斷（專案看不到、`time_entries_visibility` 不允許、所在 issue 看不到）。為了區分「模組關了但本來看得到」（仍回 `module_disabled`）與「本來就看不到」（`not_found`），0023 把 SQL 函式拆成 `time_role_visibility(project, viewer)`（只看角色，不看模組）與 `time_visibility`（模組開著才回前者）。
- M1.4-D21 **其他修正**：`/me/timesheet` 只列啟用 `time_tracking` 模組的專案；日上限錯誤的 `remaining` 對 agent 一律不給（委派者的當日合計跨專案）；判斷「看得到整天」時排除封存 issue 上的工時。

### 測試

- `m1_4_time.rs`（17 個）：T-M1-31（權限矩陣）、32（可見性）、33（日上限並行）、34（`remaining` 只在看得到時給）、35（T1–T7）、36／37（文件結構、不碰 issue `lock_version`）、38（agent 寫入、預設 review、配額、scope）、39／39a（`time_list`、`read_own_time` 開關）、39b（沒 ack 建不起來）、40（`spent_hours`：詳情、`get` fields、Filter、`/filter-fields`、視圖欄與合計、agent 錯誤、MCP `time_list` 函式）、41／42（undo 與遮蔽）、43（跨日、改人；100 並行的鎖順序壓力測試，用 `pg_stat_database.deadlocks` 前後比對）、44（報表與週表）、待審佇列、CHECK 清單與常數一致、HTTP 路由。
- `m1_4_upgrade.rs`：0022 → 0023：每個專案 epoch 恰好 +1、權限對照、模組與設定列、委派不變、工作脈絡隔離、5 個預設活動。
- 改了既有測試：`m1_0_upgrade`、`m1_2_upgrade`、`m1_3_upgrade` 升級後改成 `run_to(20／21／22)`（原本 `run()` 會連 0023 一起跑，0023 的 epoch bump 讓「升級不 bump epoch」的斷言失敗；各自只驗自己的 migration）；`s6_direct` 的 `available_commands` 多了 `log_time`、`update_time_entry`；`m1_1_filter`（`spent_hours` 不再是 unknown；`/filter-fields` 有它）、`m1_2_views`（`spent_hours` 欄與合計不再是 unknown）；所有 `DelegationInput` 字面值補 `read_own_time: false, read_own_time_ack: false`。
- 獨立驗證後補的（`m1_4_time.rs` 再加 11 個）：
  - 驗證者的 5 個重現測試（`vfy_*`：agent 的 `remaining`、遮蔽後改再遮蔽、undo skip、agent 在 `or` 裡用 `spent_hours`、送審模式的 agent 範圍）。
  - `t43_lock_order_two_documents`：兩份文件在同一個人的兩天之間反向移動，第一份鎖完第一列就停（fault point），第二份等鎖後再放行；比對 `pg_stat_database.deadlocks`。已用 mutant 確認：把鎖的順序改成「先鎖減少的那天」時這個測試失敗（deadlock 1 次），`t43_lock_order_stress` 仍通過。
  - `agent_log_time_quota`：成功的 direct `log_time` 寫一筆 `time_write`，第 21 次 `quota_exceeded`。
  - `t41_undo_permission_and_skip`：`permission` 原因、`created` 只要 `log_time`、`skip_time_entries` 部分還原後剩下的欄位仍可 undo。
  - `update_delete_invisible_and_closed`：私有 issue 上的工時、`own` 看不到別人的（模組關了也是 `not_found`；看得到的人是 `module_disabled`）、update 的 `closed_issue`。
  - `agent_view_run_with_spent_hours`、`t39c_mcp_time_list`（經 MCP 協定）；`m1_4_http` 加 `POST /time-entries/{id}/redact`、`PUT /delegations/{id}/read-own-time`（開關各 +1、bob 改不了、step-up 過期拒絕）。
- `m1_4_upgrade.rs` 加：所有角色 `time_entries_visibility = 'all'`；`m1_4_fresh_install_matches_migration`（全新安裝跑完 migration 再 `seed-demo`，內建角色的 M1 權限與 0023 給既有角色的一致）。
- 沒測到：委派精靈的警告只驗證 API 回應的文案，畫面要等前端接（契約要求截圖檢查）。

## M1.5 疑點與照建議的做法

- M1.5-D1 **規則版本字串**（附錄 C R13）：`r{site}.{project}.s{L0}.w{workflow}`。`site`、`project` 是兩層已發布的規則版本號（0 = 沒有規則）；`L0` 是 0025 新加的 `org_settings.form_l0_version`，必填、停用、適用範圍、可見性、刪除中、選項出現／消失／停用時由 trigger +1（改名、排序、說明不動）；`w` 是既有的 workflow 版本。版面另有 `layout_version = l{site}.{project}`，所以只改版面時 `rules_version` 不變（T-M1-49）。
- M1.5-D2 **一份文件動到多個 (專案, tracker)**：版本字串是各組的字串依 (專案, tracker) 排序去重後用 `,` 接起來；沒有 issue 目標（例如只有工時）時 `validate` 的 `rules_version` 是 `null`。
- M1.5-D3 **`expected_rules_version` 不進冪等 hash**：它是送出時的條件，不是內容；不符時回 `form_rules_changed`（409，`details {expected, current}`），算 rejection（不寫 operation 結果、不留 changeset）。
- M1.5-D4 **`readonly_if` 用交易開始時的狀態判斷**，值沒變也擋（同 workflow 的 readonly）。同一份文件建立的 issue 沒有基準，不檢查。
- M1.5-D5 **讀不到的規則**（獨立驗證後改）：呼叫者讀得到規則目標**與**所有條件欄位時才受約束，錯誤與 `describe_form` 才給 `rule_id`。目標或任一條件欄位讀不到：人不受約束（`describe_form` 也不列）；agent 被擋（`required_field_missing`／`readonly_field`／`invalid_value` 帶 `blocked: true`，不帶 rule id、不帶讀不到的欄位名）。規則提到已停用或不適用的欄位時，誰都不受約束。`allowed_values_if` 會檢查所有違反的規則，不只第一條。原本「目標讀得到、條件讀不到」仍約束人，錯誤會透露隱藏值，已改。
- M1.5-D6 **`describe_form` 的規則是陣列**：契約範例寫單一物件（`required_if: {…}`），實作是 `required_if: [{rule_id, when}]`（一個欄位可以有多條規則）。`allowed_values_if` 項目多 `values`。
- M1.5-D7 **`form_invalid` 多兩種 `kind`**：契約列了六種；另加 `malformed`（JSON 形狀不對）與 `invalid_rule`（規則各部分對不上，例如 `subject` 不能有 `required_if`、`text` 欄位不能有 `allowed_values_if`、op 和型別不合、rule id 重複）。
- M1.5-D8 **草稿**：每次 `PUT /forms/{id}/drafts/{part}` 都產生新的版本號，舊草稿變 `discarded`；`base_version` 是目前草稿的版本，沒有草稿時是已發布版本（0 = 沒有），不符回 `conflict`（`details.current_version`）。回溯（`POST /forms/{id}/rollback`）把舊版內容用新版本號再發布一次，規則照 `mode` 跑 impact，並把開著的草稿標 `discarded`（它的基底已被取代）。發布時草稿版本必須大於目前已發布版本，否則 `conflict`；發布已被丟棄的草稿也是 `conflict`（版本號只增不減，§3.6.3）。
- M1.5-D9 **impact 的計數**（獨立驗證後改）：只算呼叫者看得到的：`existing_issues.scanned`／`violating`／`sample_keys` 只含看得到的單；有看不到的單違反時不給數字，`complete: false`，`block` 模式以同一句「無法證明沒有違反」擋下（和超過 20000 張一樣）。`pending_changesets` 只列呼叫者自己（`on_behalf_of`）且所有目標都看得到的提案，判斷是近似（把文件的欄位值與 transition 套在目前的值上，不是完整模擬）；`undo_records` 只算看得到的單上 30 天內 agent 的執行（direct 或 changeset）。`accept_and_mark` 仍標記範圍內**所有**違反的單（不論發布者看不看得到）。
- M1.5-D9b **存下的 impact 只有計數**：發布時寫進版本列的 `impact` 把 `sample_keys` 與 `pending_changesets.ids` 清成空陣列，另加 `marked`；`GET /forms/{id}`、`/versions` 因此不會把發布者看得到的 key 給別人。計數仍是發布者視角。
- M1.5-D10 **`POST /admin/forms`**（契約表沒有）：建立或取得 (tracker, 專案?) 的表單列，自然冪等。表單管理與欄位管理的寫入都**不收 Idempotency-Key**，用 `base_version`／`draft_version`／`lock_version` 防並行。
- M1.5-D11 **權限**：欄位管理只給管理員；專案表單給管理員或該專案有 `manage_project` 的人；站台表單只給管理員。**不需要新權限**，0024／0025 不動 `role_permissions`，不 bump epoch。
- M1.5-D12 **0024 之前存的提案**（`changeset_versions.rules_version` 是 NULL）：有任何 L1 規則生效時才算「規則已變」（佇列不能勾）；沒有規則時不受影響。新提案在 `INSERT` 時就寫入版本（`changeset_versions` 只能新增）；版本在驗證**之前**讀（提案與 owner 修訂都是），所以驗證期間有新規則發布時存的是較舊的版本，佇列會標「規則已變」。
- M1.5-D13 **marks**（獨立驗證後改）：`accept_and_mark` 只記 rule id；`GET /issues/{key}`（`form_rule_marks`，一律是陣列）與 MCP `get`（沒有時省略）只列呼叫者讀得到所有欄位的規則。下一次動到欄位的寫入只清掉**寫入者受約束且最終狀態符合**的規則的 mark（從 `rule_ids` 移除；全部清完才設 `cleared_at`，保留原 id 當紀錄）；不受約束的人寫入不清（只加 note 也不清）。
- M1.5-D14 **`validate`**：人經 `/commands/validate` 不計配額；agent 經 MCP 每個委派每小時 120 次（超過 `quota_exceeded`）。沒有 `op_id` 的錯誤（最終狀態檢查）掛到每個 op，狀態 `blocked`。`would_require_review` 只看委派政策（目前沒有 agent_chain，M1.9 才有）。
- M1.5-D15 **自訂欄位管理**：
  - 新 `DELETE /admin/custom-fields/{id}/options/{oid}`（契約沒列）：沒人用的選項才能刪，用到時 `invalid_value`（`kind: "option_in_use"`），改用停用。
  - 改型別一律 `unsupported_operation`（`details.suggested = "conversion"`）。
  - 停用的欄位：`describe`、`describe_form`、版面與規則都不列，寫新值是 `invalid_value`，舊值仍顯示、篩選仍可用；重新啟用即恢復。
  - 改可見性或角色時，每個受影響的專案 epoch 至少 +1。**更正**：驗證實測同一個 PATCH 同時改可見性和角色是 +3、改 roles／projects／is_for_all 是 +5，原因是 0010 的 trigger 在 DELETE 與 INSERT 各觸發一次（原先寫的 +2 不對）。不改 trigger；測試維持「至少 +1」。和契約 T-M1-57 寫的「+1」不同，記為差異（**待使用者確認**）。
  - PATCH 把欄位改成受限（`visible` true→false）且已發布規則的條件提到它時，回應多 `warnings: [{kind: "restricted_condition_field", field, form_id, rule_ids}]`，不擋：讀不到它的人從此不受那些規則約束（D5）。
- M1.5-D16 **Q-S9 轉換**：`mapping` 格式：list→list 是 `{"options": {舊選項 id: 新標籤}}`；文字→list 是 `{"values": {文字: 新標籤}}`；文字→int／float／date／bool 與轉成文字是 `{}`。每批 500 張，依 issue id 排序，先依 id 順序鎖住該批 issue 再讀舊值，批次與游標同一個交易提交，所以失敗後 `POST /admin/conversions/{id}/start` 從 `failed` 續跑不會重寫；服務重啟時欄位背景工作接手 `running` 的轉換，每個轉換各自一個 task（錯誤互不影響，也不卡住索引工作；同一行程不會重複跑同一個）。開始時來源與目標欄位都不能是停用或刪除中（`conflict`，`kind: "field_inactive"`）。新欄位不適用的 issue（tracker／專案不符）不寫，計入報告的 `not_applicable`。事件 `origin = import`、`actor_kind = system`、`system_operation = cf_convert:<id>`；舊欄位不會自動停用。報告只有計數與 issue id／key 加原因代碼（`unmapped`、`unparseable`、`too_long`），不含值。
- M1.5-D17 **`issue_field_changes` 不含自訂欄位**：M1.0 的投影只收狀態、指派、版本、優先度，轉換寫的事件照規則不進去。
- M1.5-D18 **undo 的 `rules_changed`**：欄位層級的原因多 `code`（新規則給的錯誤碼）與 `rule_id`（讀得到時）；整批 undo 被規則擋下時回 `undo_not_applicable`（`reasons[{reason: "rules_changed", code, rule_id}]`）。
- M1.5-D19 **condition 的空字串**：自訂文字欄位的 `""` 視為沒有值（`not_set` 成立），和指令文件一致；conformance 檔有對應向量。
- M1.5-D20 **轉換中的舊欄位唯讀**（**待使用者確認**）：轉換在 `running` 時，寫入舊欄位（值有變）回 `invalid_value`（`kind: "field_converting"`）；轉換結束（`succeeded`／`failed`）就解除。完成後是否停用舊欄位仍由管理員決定。`failed` 時不凍結：期間寫進舊欄位、而游標已經過的單，續跑不會再轉。
- M1.5-D21 **非管理員寫規則**：專案經理寫或發布規則時，欄位目錄只含他讀得到的欄位；讀不到的欄位不論在目標或條件都回 `unknown_field`（不留存在性 oracle）。`restricted_condition_field` 只對讀得到該欄位的人出現。管理員看到全部。版面檢查仍用完整目錄。
- M1.5-D22 **版面**：`hides_required` 也看 workflow 的必填（任何角色在某狀態設 `required`，就當作「狀態是那個時必填」）；`describe_form` 的版面區塊如果欄位全被濾掉，整塊不回（不回標題）。

### 測試

- `m1_5_forms.rs`（31 個）：
  - `t45_required_if_on_every_entry_point`：`/commands`、MCP `command_submit`（經協定）、`validate`（人與 agent）、ChangeSet 套用、undo 五個入口都回 `required_field_missing` 與 `rule_id`；同時驗 T-M1-52（佇列 `rules_changed`、`review_modes` 只有 detail、queue 模式核准回 `review_outdated`、逐份核准用新規則失敗）與 T-M1-53。
  - `t46_allowed_values_and_t47_readonly_baseline`：T-M1-46、T-M1-47、T-M1-55。
  - `t48_form_invalid_before_publishing`：`restricted_condition_field`、`condition_too_complex`（三層、六項）、`unknown_field`、`hides_required`（`subject` 與發布後的 `required_if`）、`cycle`。
  - `t49_t50_expected_rules_version`：只改版面時舊版本仍成功；改規則後 agent、人、`validate`、MCP `validate` 都回 `form_rules_changed`；版面回溯。
  - `t51_publish_modes_and_marks`：impact、`block` 被擋、草稿版本過期 `conflict`、`accept_and_mark` 寫 marks、note 不清、通過的寫入清掉、非管理員不能管理。
  - `t54_validate_writes_nothing`：1000 次 `validate` 後 `operation_requests`、`changesets`、`issue_events`、`lock_version` 都沒變；agent 第 121 次 `quota_exceeded`。
  - `t56_conformance_vectors`：`api/form-conformance.json` 49 個向量全部一致（伺服器端；前端評估器要另外跑同一個檔）。獨立驗證後補：明確 null、`ne`／`in` 遇缺值、大小寫（id 不分、字串分）、停用的選項、看不到的欄位（等同缺值）。
  - `describe_form_leaks_nothing_unreadable`：讀不到的欄位、選項、rule id 都不出現在 `describe_form`、版面、錯誤、`validate`、議題頁 marks；讀得到的角色看得到；agent 被擋時不帶名稱。
  - `mcp_describe_form_and_validate`：經 MCP 協定的 `describe_form`（create、update）與 `validate`（valid、invalid、`would_require_review`）。
  - `t57_visibility_quarantines_agents`、`field_lock_version_and_errors`（兩個 PATCH 同時送只有一個成功、`unsupported_operation`、`required_impact`、`name_taken`、非管理員、`option_in_use`、停用的選項不能寫）、`t58_conversion_with_failure_and_resume`（string→int 預檢 3 張、注入失敗後續跑、事件與值各一次、list→list 對照、轉換中刪舊欄位 `conflict`）、`t59_deactivated_field`。
  - `check_lists_match_constants`：0024／0025 的 CHECK 與程式常數一致。
  - `m1_5_http`：經 router 的表單、表單管理、`/commands/validate`、欄位管理（201、409、202 與 `Location`）、轉換預檢、議題頁 `form_rule_marks`。
  - 獨立驗證的重現（`v1`…`v7`）：impact／block 不透露看不到的單、經理不能替讀不到的欄位寫規則、條件欄位發布後改受限、回溯後發布舊草稿、R19 `then_required` 含 L1、存下的 impact 不含 key、不受約束的寫入不清 mark。
  - 補的缺口（`g_*`，各自用 mutant 確認會失敗）：條件讀不到時不約束人、agent 被擋且不帶 rule id、PATCH 警告（`g_condition_unreadable_target_readable`）；0024 前 NULL 版本（`g_pre_0024_proposal_counts_as_changed`）；impact 只列自己的提案（`g_impact_pending_only_own_proposals`）；發布等寫入的 FOR SHARE（`g_publish_waits_for_write_in_flight`，用 `apply.after_locks` 擋住套用）；改 workflow 版本會變（`g_workflow_change_moves_rules_version`）；發布、回溯、轉換開始、刪欄位經 HTTP 都要 step-up（`g_step_up_over_http`）；Triage accept 與 log_time 混合文件經過 L1（`g_triage_accept_and_log_time_mixed_checked`）；轉換凍結舊欄位、不適用的單跳過、停用欄位不能開始（`g_conversion_guards`）；workflow 必填與全濾掉的區塊（`g_layout_workflow_required_and_hidden_sections`）。
- `m1_5_upgrade.rs`：0023 → 0025：epoch 不變、工作脈絡仍可用、既有欄位 `lock_version = 0`、`form_l0_version` 從 1 開始、L0 trigger 只在該動時動（改名、選項改名不動；必填、停用、選項停用、角色、專案、is_for_all、tracker、可見性、選項增刪、刪除中各 +1）、表單列動 `schema_version`、站台表單每個 tracker 一個。
- 改了既有測試：`s6b_queue` 的 `eligibility` 多一個參數。`m1_4_upgrade` 原本改成 `run_to(23)`，驗證者實測 `MIGRATOR.run` 可過，已改回（**待使用者確認**）。
- 沒測到：前端的 `show_if` 評估器（前端未接）；`resume_conversions` 只在續跑路徑間接測到，沒有模擬服務重啟。