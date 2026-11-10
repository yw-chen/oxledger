# M1 可日用：檢查清單與疑點

範圍：設計 05（`docs/design/05-m1-daily-use-contract.md`）§8 的 M1.0–M1.11。本文沒寫的共通規則沿用 05 與 02。

## 階段

| 階段 | 內容 | 完成標準 | 狀態 |
|---|---|---|---|
| M1.0 前置 | 0019、0020；`perm.rs` 權限清單；`estimated_hours` 進命令引擎、describe、事件、undo、政策；`issue_field_changes` 寫入與回填；D12 遮蔽刪投影 | 既有測試全綠；`estimated_hours` 走完 02 §16.7b 對應案例；T-M1-03 | 完成（2026-11-09，全套兩輪全綠，未 commit） |
| M1.1 Filter AST v2 | `filter.rs` 擴充成 v2（內建欄位、`me`、相對日期、`ever_in`、排序）；舊參數轉 v2；`/filter-fields`、`describe filter_fields`；`search` 收 `where` | T-M1-01…05；E4 可見性測試加 v2 隨機條件；OpenAPI 與 `api/types.ts` 重產 | 完成（2026-11-10，含獨立驗證修正；全套兩輪全綠：test-support 197 過、test-support,github 205 過；未 commit） |
| M1.2 視圖與看板 | 0021；`/views*`；內建視圖；`run`／`preview`；看板計數；專案分頁 `view`、`board`、`triage`；MCP `view_list`、`view_run` | T-M1-06…20；前端可接列表與看板 | 實作完成，含獨立驗證 9 項修正（2026-11-10；`m1_2_views` 26 個、`m1_2_upgrade` 1 個測試；全套兩輪全綠：test-support 226 過、test-support,github 234 過；前端未接；未 commit） |
| M1.3 Triage | 0022（含 `self_triage`、`same_sponsor`）；建立時進收件匣；`triage_decide`；設定；`/triage*`；Filter `triage_self_decided`；MCP `triage_list`；政策與委派頁「新操作」 | T-M1-21…30c；收件匣「最近決定」可用 | 實作完成，含獨立驗證後的修正（2026-11-10；`m1_3_triage` 6 個、`m1_3_upgrade` 1 個測試；前端未接；未 commit）。**0022 不給任何角色 `triage_issues`，M1.4 給 Manager** |
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
