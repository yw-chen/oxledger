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
| M1.6 首頁與介面角色 | 0026；`/me/*`、範本；首頁小工具資料 | T-M1-60、61 | 實作完成（2026-11-10；`m1_6_home` 6 個、`m1_6_upgrade` 1 個、`home` 單元測試 2 個；前端未接；未 commit）。**0026 不動權限、不 bump epoch** |
| M1.7 搜尋 | 0027；BM25F 欄位與 fast field；別名展開；概念管理；藍綠重建切換 | T-M1-62…65；E1 nDCG＠10 不低於 0.810；E2 用獨立查詢集量別名增益（只記錄） | 實作完成（2026-11-10；`m1_7_search` 12 個、`m1_7_upgrade` 1 個、E1 單元測試 2 個；獨立驗證 D1–D7 已修；前端未接；未 commit）。**0027 不動權限、不 bump epoch**；索引自動藍綠重建。E1 不展開 0.809525（固定；門檻 0.8095 = M0 四捨五入，待確認）；E2 受阻（沒有獨立查詢集） |
| M1.8 通知 | 0028；`notify` consumer；`/notifications*`、`/issues/{key}/watch`、`/me/notification-preferences`；GC 與 lag 監控；保留清除 | T-M1-70…83、125、126、132；派送延遲 p95 ≤ 10 秒（本機 1 萬張單、50 人）；前端鈴鐺、通知中心、關注、偏好可用 | 後端實作完成，獨立驗證 D1–D7 已修（2026-11-11；`m1_8_notifications` 32 個＋效能 1 個（`#[ignore]`）、`m1_8_upgrade` 1 個；前端未接；未 commit）。**0028 不動權限、不 bump epoch**。派送延遲 p95 5.45 秒（加上收件人鎖之後；本機、10 人同時寫入 600 個事件，量法見 M1.8-D15） |
| M1.9 AI 團隊 | 0029；runtime、角色 principal、共用脈絡；token 綁 principal；principal／專案層暫停；鏈上限；`/ai-team*`、`/me/agent-runtimes*`；角色範本；`describe agent_context` | T-M1-84…99、66a、121、123、124、127、128、133；02 的 E4w、§9.6 受眾測試、§16.7b 全部重跑全綠；既有 agent token 升級後照常可用 | 後端實作完成，獨立驗證的缺陷 D1…D10、M14 已修（2026-11-11；`m1_9_ai_team` 32 個、`m1_9_upgrade` 2 個；全套 test-support 與 test-support,github 全綠，含 s8_gate 的 E4w-2…6、s8_gate_extra、s6_audience、m1_0_estimated_hours；前端未接；未 commit）。**0029 不動權限、不 bump epoch**。待使用者決定：qa 範本的 transition 白名單（M1.9-D6）、pm_assistant 改 subject 走 review（M1.9-D7）、`oxledger-ai@…` 外觀 handle 要不要自動改名（M1.9-D15） |
| M1.10 右欄 AI 對話 | 0030；feature `ai`；部署端點登記與 IP 驗證；兩種 provider 串流；執行層；`AuthMethod::BuiltinRun`；工時汙染；`/ai/*`、`/admin/ai/*`；D12 與保存清除；假 provider 測試伺服器 | T-M1-100…118、119、120、122、129、130、131、134；前端右欄對話與設定頁可用；真的 Anthropic key 與自架 OpenAI 相容端點各手動跑一次並記錄 | — |
| M1.11 發布門檻 | E4w 對新命令與新身分全套、T-M1-66…69、E5 子集、通知與 AI 對話的 canary 洩漏測試；T-M1-119、120、121、122、127 | 越權、繞過政策、重複效果、canary 洩漏 = 0；key 送往非登記主機 = 0、內建 AI 讀工時後的 direct 寫入 = 0；核心情境正確率 ≥ 90%；`upgrade-notes.md` 寫好 0029、0030 | 實作完成（2026-11-11；獨立驗證後修正 D1–D8：`m1_11_gate` 13 個、`m1_11_gate_ai` 7 個、`m1_11_e5` 1 個＋1 個 ignored）。各項 = 0 都由測試斷言；**核心情境 ≥ 90% 還沒驗證**，要使用者提供真的 key 與端點跑 `e5_live`（`m1-11-e5.md`） |

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

## M1.6 疑點與照建議的做法

- M1.6-D1 **權限**：不需要新權限，0026 不動 `role_permissions`、不 bump epoch。`/me/preferences`、`/me/home`、`/me/home/apply-template`、`/home/templates` 只給登入的本人（human session）；agent 在服務層是 `human_session_required`，REST 帶 bearer 是 `unauthenticated`。沒有任何端點帶使用者 id（T-M1-61）；MCP 沒有相關工具。
- M1.6-D2 **兩個樂觀鎖**：DDL 多一欄 `home_lock_version`。契約範例 `/me/preferences` 的 `lock_version` 是 3、`/me/home` 是 5，顯然是分開的；共用一個鎖會讓「改主題」與「另一個分頁編輯首頁」互相 `conflict`。`apply-template` 只 bump 有變的那一邊（重複 merge 不 bump）。
- M1.6-D3 **冪等**：三個寫入都收 `Idempotency-Key`（共用 `operation_requests`，action `me_preferences_put`／`me_home_put`／`me_home_apply_template`）；重送回目前狀態，同 key 不同內容 `idempotency_key_reused`。`GET` 首次建列（`INSERT … ON CONFLICT DO NOTHING`，單獨的短交易）不收 key。`conflict` 的 `details` 帶 `current_lock_version` 與 `current`（前端「已載入最新版本」直接用）。
- M1.6-D4 **`PUT /me/preferences` 是部分更新**：除了 `lock_version`，沒帶的欄位不變；`nav_shortcuts`、`default_views` 帶了就整份取代。`onboarded: true` 記下第一次的時間，`false` 清掉（下次登入再問）。`interface_roles` 去重、保留順序。
- M1.6-D5 **小工具格式**：`{id, type, width, view_id?, limit?, project_id?}`；`id` 是 1–40 個英數、`_`、`-`，新的可以不給（伺服器產生 `w_xxxxxxxx`）；`width` 預設 `need_decision` 2、其他 1；`view` 的 `limit` 預設 8。輸出多 `protected`、`available`、可用時的 `view_name`／`view_kind`（view）與 `project_key`／`project_name`（milestones）；這些唯讀欄位 PUT 時可以原樣送回（忽略）。錯誤 `invalid_value` 的 `kind`：`protected_widget_missing`、`duplicate_protected_widget`、`too_many_widgets`、`unknown_widget_type`、`invalid_widget_id`、`duplicate_widget_id`、`width_out_of_range`、`limit_out_of_range`、`view_required`、`unexpected_property`、`unknown_view`、`unknown_project`。
- M1.6-D6 **每次讀取重驗參照**（任務第 3 點）：`view` 小工具、`view` 捷徑、`default_views` 的視圖用 `views::usable`（同 `GET /views/{id}` 的 `load` 加 R-VD10 的綁定專案檢查）；`milestones` 用 `versions::readable`（同 `GET /projects/{id}/versions` 的 `access`：專案在可見範圍、未封存、`view_issues`）。視圖被刪、改成看不到、專案被移出、模組停用、權限撤銷時，小工具一律 `available: false`，不給名稱、不給數量，被刪與看不到的回應欄位完全一樣；`default_views` 的失效項目（獨立驗證後改）照樣回傳，形狀比照捷徑：`{view_id, available: false}`，不帶名稱，只回自己存的 id；原樣送回 PUT 時保留（不重驗），所以前端整份 PUT 不會默默刪掉。被刪與看不到仍完全一樣。小工具本身不帶任何數量；數量只來自各自的資料端點（`/views/{id}/run`、`/triage/counts`、`/projects/{id}/versions?include=stats`、`/me/timesheet`、`/home/agent-summary`），它們本來就逐項依讀者過濾。
- M1.6-D7 **PUT 時的參照**（獨立驗證後補）：視圖 id 一律存 `views::usable` 回傳的正規 id（列 id，或 `builtin:project_board:<專案 uuid>`），送進來的 view id 超過 80 字元回 `invalid_value`（`kind: "invalid_view_id"`）；小工具保留與否比對的是**同 id 且同 view_id**（同 id 換視圖要重驗）。新的或改過的視圖／專案必須現在讀得到，否則 `unknown_view`／`unknown_project`（不分不存在與看不到，訊息與 details 相同）；和既存**同 id、同參照**的小工具（或相同的捷徑、相同的 default view 項目）不重驗，讓視圖消失後仍能重排首頁；但失效的參照不能搬到別的小工具 id。`milestones` 的 `project_id` 收 id 或 key，存 id。
- M1.6-D8 **範本**：契約表的「與我相關」＝`builtin:involving_me`、「指派給我」＝`builtin:assigned_to_me`。**通用範本**（沒選角色）＝需要你決定、指派給我、與我相關（契約沒列內容）。PM 範本的 `milestones` 沒有 `project_id`（範本不知道專案；前端請使用者選，沒選時 `available` 是 `true`）。複選依**請求的順序**輪流取、重複只取一次、最多 8 個；捷徑同樣輪流去重；`*` 預設視圖取第一個角色的。範本是程式常數（`home.rs` 的 `role_template`）。
- M1.6-D9 **`apply-template` 也套捷徑與預設視圖**：`replace` 換成範本的小工具（使用者自己的 `need_decision` 保留 id 與寬度）、換掉捷徑、設 `*` 預設視圖（各專案的項目保留）；`merge` 只加缺的小工具（同 type＋view_id＋project_id 算同一個，上限 12）、缺的捷徑，`*` 沒有時才設。回應 `{home, preferences}`。理由：§3.8 範本表三欄都是「預設」，UI 的「套用新預設」應一次套齊。`interface_roles` 本身仍由 `PUT /me/preferences` 改（§6.12 的流程）。
- M1.6-D10 **`default_views`**（獨立驗證後改）：key 是專案 id 或 `*`（其他所有專案）；值是視圖 id 或 `builtin:list`／`builtin:board`／`builtin:timeline`（專案的內建列表、看板、時程分頁）。專案項目的視圖必須**綁定在同一個專案**，否則 `unknown_view`；`*` 只收這三個 builtin 或沒綁專案的視圖（綁了專案的是 `invalid_default_view`）。key 正規化（uuid 小寫）後重複回 `invalid_value`（`kind: "duplicate_default_view"`）。最多 100 項。讀取時每項是 `{view_id, available, name?}`；PUT 收字串或同形狀物件（`available`、`name` 忽略）。
- M1.6-D11 **捷徑**：`{kind, id?}`，kind 是 `view`（要 `id`）、`triage`、`timeline`、`board`、`timesheet`；最多 20、不能重複。讀取時每項帶 `available`（view 可開時另給 `name`）。
- M1.6-D12 **`GET /home/templates`**：`roles` 是逗號清單，未知角色回 `invalid_argument`（`kind: "unknown_role"`，查詢參數）；`apply-template` 的未知角色是 `invalid_value`（body）。
- M1.6-D13 **M1.5 遺留修正**：非管理員（專案經理）寫表單**版面**時，欄位目錄也用依可讀性過濾的目錄（`author_catalog`）：把讀不到的欄位放進版面、或寫進 `show_if`，一律 `unknown_field`，不再出現 `field_not_applicable`、`hides_required` 之類會透露該欄位的錯誤。「什麼是必填」仍用完整目錄（只影響已放進版面、也就是讀得到的欄位）。管理員不變。M1.5-D21 的「版面檢查仍用完整目錄」作廢。
- M1.6-D14 **改規則時重檢已發布版面**（獨立驗證後改，**待使用者確認**）：原本用完整目錄對已發布版面重跑整個 `parse_layout`，版面裡的欄位之後被停用或不再適用時，所有規則草稿與發布都被 `unknown_field` 卡死（管理員也是；站台表單會被任一專案版面卡住），錯誤還帶出經理讀不到的 key。改成：已發布版面以寬鬆方式讀（停用、不適用、未知的欄位略過，和顯示時一樣；`show_if` 解析不了算「可能隱藏」），只比較「改規則前」與「改規則後」被 `show_if` 隱藏卻必填的欄位，**只有這次變動新造成的**才回 `hides_required`；錯誤只對讀得到該欄位的人帶 `field`，讀不到時是不帶 key 的訊息（目前經理的規則目標必然讀得到，這條是防禦）。已經存在的違反不擋。
- M1.6-D15 **版面 impact 的 `layout_warnings`**（獨立驗證後補）：`POST /forms/{id}/drafts/layout/impact` 的「未放進版面」警告也改用作者目錄，經理不會看到讀不到的欄位；管理員不變。
- M1.6-D16 **milestones 的 `versions::readable`**：獨立驗證指出 PUT 時拿掉這個檢查仍通過測試（封存專案本來就不在 `visible_project` 的範圍），目前等價於可見範圍檢查；保留當防禦（`view_issues` 不在範圍內時的保險），記為等價 mutant。

### 測試

- `m1_6_home.rs`（6 個）：
  - `t60_protected_widget_and_templates`：T-M1-60（缺 `need_decision`、空清單都是 `protected_widget_missing` 且不寫入；其他 8 種錯誤；12 個上限；移動與改寬；replace 保留自己的 need_decision、merge 只加缺的、重複 merge 不 bump、滿 12 個時 merge 不加；三個角色 ≤ 8；壞的 mode／role；範本端點不改任何東西）。
  - `preferences_put_and_concurrency`：首次登入流程、六種驗證錯誤；兩個同 `lock_version` 的 PUT 同時送（`tokio::join!`）恰好一個成功、另一個 `conflict` 帶目前版本；勝者的 key 重送不 bump、換內容 `idempotency_key_reused`；首頁同樣兩個並行 PUT 只成功一個。
  - `t61_nobody_reads_anyone_elses`：T-M1-61（bob 只讀到自己的；agent 四個入口 `human_session_required`；registry 沒有相關工具；openapi 的相關路徑都在 `/me/` 或 `/home/` 且不帶參數；經 router 試 6 種帶 alice id 的路徑都是 404；查詢參數帶別人被忽略；bearer 401）。
  - `references_rechecked_on_every_read`：新參照讀不到時 `unknown_view`／`unknown_project`，「看不到」與「不存在」的 code、訊息、details 完全相同；離開 AUTH（AUTH 綁定的個人視圖、AUTH 看板、AUTH 里程碑、AUTH 預設視圖、捷徑）、視圖被刪、WEB 停用 `issue_tracking`、撤銷 `view_issues` 四種情況各自變暗且不帶名稱，被刪與看不到的欄位集合相同；資料端點同樣看不到（`views::run`、`versions::list`、`triage::counts` 不再列 AUTH）；失效的參照原樣保留可重排、搬到新 id 被擋；重新加入後恢復。
  - `check_lists_match_constants`：0026 三個 CHECK 與 `home::ROLES`／`THEMES`／`DENSITIES` 一致；agent principal 不能有列。
  - `m1_6_http`：經 router 的 GET／PUT／POST、缺 Idempotency-Key 400、重送不 bump、未知欄位 400、GET 的回應原樣送回 PUT、移除 need_decision、`conflict` 409、`/home/templates`。
  - mutant 確認（各自讓測試失敗後還原）：不擋缺 need_decision、PUT 不驗新視圖、讀取不重驗 view 小工具、讀取不重驗 milestones、`default_views` 不濾看不到的專案。
- 獨立驗證的重現（`verify_m16_*`，7 個附加到 `m1_6_home.rs`；V1 改成要求超長 id 回 `invalid_view_id` 並驗證短的變體存正規 id）；`m1_5_forms.rs` 加 F1、F2（F2 預期改成經理存無關規則成功）與 `g_rules_recheck_only_new_hides_required`。
- 獨立驗證後的 mutant（各自讓測試失敗後還原）：impact 改回完整目錄（F1）、重檢改回嚴格 `parse_layout`（F2、`g_rules_recheck…`）、不限 view id 長度（V1）、小工具不存正規 id（V1）、不擋重複 key（V2）、小工具只比 id（V5）、default view 不檢查綁定專案（V6）、GET 省略失效的 default view（references 測試）。沒有對應 mutant 的：D14 的「只算新造成的」差集（沒有測試建立「改規則前就已違反」的版面）、讀不到欄位時不帶 key 的分支（目前到不了）。
- `m1_6_upgrade.rs`：0025 → 0026：epoch 與 `schema_version` 不變、工作脈絡仍有效、表是空的、預設值、CHECK 與 user-only trigger（INSERT 與 `UPDATE OF user_id` 改成 agent 都被擋）、寫入偏好不動 epoch。
- `home` 單元測試 2 個：範本輪流取的順序、replace／merge 的 id 衝突處理。
- `m1_5_forms.rs` 加 `g_manager_layout_unreadable_field`（M1.6-D13；mutant：版面改回完整目錄時，經理放讀不到的欄位會成功、測試失敗）。
- 既有升級測試沒改（`m1_4_upgrade` 的 `MIGRATOR.run` 會連 0026 一起跑，0026 不 bump epoch）。
- 沒測到：前端畫面（首次登入對話框、編輯首頁、以角色預覽）未接；`my_time_week` 的模組過濾沿用 M1.4 的 `/me/timesheet` 測試，沒在 M1.6 重測。

## M1.7 疑點與照建議的做法

- M1.7-D1 **權限**：不需要新權限，0027 不動 `role_permissions`、不 bump epoch。概念的新增、改名、退役、標籤新增與改狀態只給管理員（`is_admin`，human session；agent 走 `commands::executor` 被擋）；非管理員是 `forbidden_op`（`reason: permission`）。`GET /admin/concepts*` 給所有登入的人，但非管理員只看到 `active` 概念的 `active` 標籤（`staging` 與退役概念對他們是 `not_found`／不列出），因為 staging 標籤常是還沒審過的內部用語。
- M1.7-D2 **正規化（R18）**：用 PostgreSQL 18 的 `casefold(normalize(x, NFC))`（兩者都是 IMMUTABLE），unique index 與應用程式比對用同一個 SQL 式子，不在 Rust 端正規化，所以沒有新依賴。`casefold` 跟資料庫 collation（目前 libc `en_US.utf8`：ß 不會變成 ss），和 views、versions、project tabs 的名稱規則一致；要完整 case folding 可以改成 `COLLATE pg_unicode_fast`，但要整個專案一起改，列為待決定。
- M1.7-D3 **既有撞名（R18）**：migration 不失敗，每組正規化後相同的標籤留一個 `active`（`pref` 優先，再來是 uuidv7 最舊的概念、bytewise 最小的標籤），其他改 `staging` 並 `RAISE NOTICE` 筆數。同一個概念內大小寫不同的兩個標籤也算撞名。替代做法是讓 migration 失敗要求人工清理，但升級會卡住。**（獨立驗證 D1 後補）** 之後修復名稱：每個概念只留一個 `pref` 且必須是 `active`（被降級的或第二個 `pref` 改 `alt`）；沒有啟用 `pref` 的概念挑一個剩下的啟用標籤（`alt` 優先、bytewise 最小、1–60 字元）當名稱；沒有任何啟用標籤的概念改 `retired`。服務層也防禦：概念的標籤清單一律由 `concepts.pref_label` 產生 `pref` 項；改名時沒有 `pref` 列就先 `ensure_free` 再 INSERT 一列啟用的 `pref`。
- M1.7-D4 **展開規則**：查詢的候選片段是「整個 q」加上「空白分隔的每個詞」（去重，最多 16 段）；片段正規化後等於某個 **啟用概念的啟用標籤**（含 `hidden`）時，加入該概念其他啟用標籤；每次查詢最多加 32 個標籤，已經等於某個片段的不再加。展開詞權重 ×0.5（`EXPANSION_WEIGHT`，E1 研究的 alias0.5 與 alias1 差距小，取保守值）。展開在查詢時做，改概念不用重建索引。片段不做子字串比對（「無法登入問題」不會因為包含「無法登入」而展開），避免中文長句誤展開；E1 的別名表用「包含」，所以本實作在 E1 上的展開題數比研究少。
- M1.7-D5 **`hidden` 標籤**（獨立驗證 D6 後改，**待使用者確認**）：和 `alt` 一樣參與比對與展開（拼錯字、內部用語），但非管理員的 `GET /admin/concepts*` 不列出、`?q=` 也不能用它找到概念；`match.expanded` 遇到 hidden 標籤時 `label` 改顯示概念的偏好標籤（同一組 `{from, via, label}` 去重）。原做法（非管理員看得到、expanded 列出錯字）會外露內部用語。和契約的差異：契約只說 hidden，沒說誰看得到。
- M1.7-D6 **`match.expanded`**：只列「展開標籤裡有原查詢沒有的完整詞，而且該詞在這張 issue 的命中欄位裡出現」的展開，形狀 `{from, via, label}`（`from` 是使用者打的片段、`via` 是概念的偏好標籤、`label` 是加入的標籤）；沒有時省略欄位。
- M1.7-D7 **退役概念仍佔名稱**：`retired` 概念的 `active` 標籤仍算在 unique index 裡，新概念不能拿走同名標籤（之後恢復概念不會撞名）。要讓出名稱，先把標籤改 `staging`。
- M1.7-D8 **偏好標籤**：`pref` 標籤就是概念名稱，不能改 kind 或改成 staging（`invalid_value`、`kind: pref_label_fixed`）；改名用 `PATCH /admin/concepts/{id}` 的 `pref_label`（舊名稱同時被移除）。每個概念最多 50 個標籤，標籤 1–60 字元。同一概念內重複是 `invalid_value`（`label_exists`／`duplicate_label`，422），撞到別的概念才是 `concept_label_taken`（409，`details` 帶 `label`、`concept_id`、`pref_label`；只有管理員會看到）。
- M1.7-D9 **冪等與並行**：比照 M1.5 的欄位管理，概念 API 不收 `Idempotency-Key`；`PATCH /admin/concepts/{id}` 要 `lock_version`（標籤異動也會 bump），標籤操作鎖概念列（`FOR UPDATE`）序列化；最後一道是 unique index（`23505` 轉成 `concept_label_taken`）。概念變更不寫 `issue_events`（站台設定，不是 issue 歷史）。
- M1.7-D10 **BM25F 的實作**：每個詞對每個欄位各一個 BM25 `TermQuery` 乘欄位權重後加總（Should），和 M0 的 subject／description 做法一樣，不是單一合併長度正規化的 BM25F；權重 key 5.0、subject 3、description 1、cf_text 0.7、notes 0.5（key 與 cf_text 契約沒給數字，是推論）。key 用整串（小寫）raw tokenizer，查詢每個空白分隔片段去掉頭尾標點後整串比對（`auth-12` 命中，`auth` 不命中）。
- M1.7-D11 **哪些來源進索引**：所有 key（目前的與搬移前的）、公開且未撤回未遮蔽的筆記、`visible = true` 且 `index_state <> 'dropping'`、適用於該 issue 的 tracker 與專案的 string／text／link 自訂欄位值。私有筆記與受限欄位不進索引（有權限的人也搜不到）——Q-S2 的保守做法，避免要在索引裡做逐欄位 ACL。
- M1.7-D12 **證據**：命中筆記時證據的 `field` 是 `note` 並帶 `note_id`，`citation_id` 指向 `issue_note` 來源（`verify_citations` 原本就支援，包含私有筆記與可見性的檢查）。key 與自訂欄位命中沒有 snippet（現在沒有對應的引用來源單位），`match.fields` 會列出 `key`／`cf_text`。
- M1.7-D13 **SQL 重驗涵蓋新來源**：`search_rev` 改 v2，雜湊包含 subject、description、key、自訂欄位值與公開筆記（依索引的規則從 DB 讀出）；不一致時用目前的文字比對（`Texts::survives`），所以筆記改私有、被遮蔽、欄位改受限、不再適用時，在索引同步之前就不會命中（T-M1-62、63 都有「改了、還沒 sync」的斷言）。
- M1.7-D14 **outbox**：0027 為 `issue_notes`、`issue_keys`、`issue_cf`、`field_defs`（visible／is_for_all／dropping）、`field_trackers`、`field_projects` 加 trigger，發 `topic = 'issue'`。正確性不靠它們（有 SQL 重驗），它們讓排序跟上。同一交易同一 issue 只發一筆（`outbox_issue_once`，`issues_enqueue` 也改用它），維持 S4「一個命令一筆 outbox」（`s4_commands` 驗證）。
- M1.7-D15 **版本與藍綠升級**：schema 2→3、query semantics 1→2、analyzer 不變。新程式認得 legacy payload（2／1／1），照舊語意服務（只查 subject／description、不展開、重驗只看 subject／description 與 v1 rev），背景自動重建（`needs_upgrade` + `claim_upgrade`，失敗 60 秒後再試）；切換在既有的 `catch_up_and_switch`。舊 generation 永遠不會用新語意查。status／priority 過濾在 legacy generation 上視為全部（`Not` 底下有它們時整個視為全部，保持「超集」）。
- M1.7-D16 **status／priority fast field**：以 STRING term 實作，`where` 的 `status`、`priority` 的 `eq`、`in`、`ne`（只在沒有適用性閘門時）轉成索引端預先過濾；其他運算子仍由 SQL 過濾。
- M1.7-D17 **E1 的門檻**：契約寫「不低於 0.810」，這是 M0 研究結果 a+uni0.1 的四捨五入值（實際 0.80955）。獨立驗證 D2 發現原測試多執行緒寫入使同分文件順序不固定（約 1/3 得 0.80953）；改成單元測試的 writer 單執行緒、同分依 E1 id 排序後，連跑 10 次都是 **0.809525**（展開版 0.841755）。M0 的 0.80955 是另一種同分順序。門檻改成 M0 四捨五入到 4 位的 **0.8095**；E1 別名表當概念時斷言 ≥ 0.810。**待使用者確認門檻的解讀**。
- M1.7-D19 **`limit`**（獨立驗證 D7b）：契約沒寫概念清單的 limit 規則；HTTP 層與服務層都改成 clamp 到 1–100（和其他清單端點一致），不回錯誤。
- M1.7-D20 **升級認領**（獨立驗證 D7a）：`main.rs` 的升級工作用 drop guard 釋放 `claim_upgrade`，rebuild 失敗（等 60 秒）或 panic 都會釋放，下一輪可以重試；成功才保留。
- M1.7-D21 **刪除 API**：沒有刪除標籤或概念的 API（契約只列 GET／POST／PATCH），列為後續項；upgrade-notes 改寫成可做的處理（保留 staging、改 hidden、改名、重新啟用）。
- M1.7-D18 **E2 受阻**：E2 要的「獨立查詢集」不存在（`experiments/` 只有 E1 的 60 題與別名雛形、E9、sd），沒有自行編造。上面 E1＋別名表的數字只是參考，不是 E2。

### 測試

- 獨立驗證後追加（`m1_7_search.rs` 共 12 個）：`concept_without_pref_row_lists_and_renames`（D1：缺 pref 列的概念，carol 讀到的標籤含名稱且只有啟用、非 hidden；改名後有啟用的 pref 列、新名稱會展開、撞名被擋）、`hidden_labels_stay_hidden`（D6）、`legacy_index_ignores_status_prefilter`（D3：legacy 上 status 的 ne／not_in／eq／in 都得到 1 筆）、`legacy_generation_on_disk_is_opened`（D4：磁碟上的 schema 2 generation 被開成 legacy、不重建、可搜、`needs_upgrade`）；`t_m1_64` 補 D5（staging 後搜 sso 只有字面命中、沒有 expanded）。mutant：刪掉 legacy 的 `Not` 分支 → D3 測試失敗；`Layout::of` 只認 Current → D4 測試失敗（都已還原）。`m1_7_upgrade` 加 c4（pref 被降級、alt 補位）、c2 全部降級後 `retired`、每個啟用概念恰有一個與名稱相同的啟用 pref。
- `m1_7_search.rs`（原 8 個）：
  - `t_m1_62_private_notes_and_restricted_fields_do_not_match`：私有筆記、`visible = false` 欄位，有 `view_private_notes` 的人也搜不到；可見欄位命中時 `match.fields = ["cf_text"]`、沒有 snippet；改成受限後**還沒 sync** 就不命中；sync 後仍不命中；欄位不再適用於 tracker 時也不命中。
  - `t_m1_63_public_notes_match_with_a_note_citation`：公開筆記命中、`match.fields = ["notes"]`、證據 `field = note` 帶 `note_id`、`verify_one` 是 `valid`；新增筆記會發 outbox；改私有、遮蔽後還沒 sync 就不命中。
  - `keys_match_whole`：小寫 key 命中、排第一、`match.fields` 含 `key`。
  - `t_m1_64_alias_expansion`：登入↔SSO，字面命中排第一，展開命中帶 `match.expanded = [{from: 登入, via: 登入, label: SSO}]`，字面命中沒有 expanded；反向、大小寫不分；改 staging 不展開、改回啟用又展開；退役概念不展開。
  - `expansion_and_concepts_do_not_leak`：carol（只在 WEB）用概念名稱、標籤、組合查都拿不到 AUTH 的 issue；agent（委派 WEB）同樣；非管理員 GET 只看到啟用標籤、退役概念 `not_found`、清單是空的；非管理員新增概念 `forbidden_op`；agent 加標籤被擋。
  - `t_m1_65_label_taken`：大小寫、NFD、前後空白都撞到別的概念的標籤，`concept_label_taken` 帶 `concept_id`、`pref_label`；新概念的偏好標籤撞名；staging 可以重複、改回啟用被擋；退役概念仍佔名稱；直接 INSERT 被 unique index 擋。
  - `legacy_index_serves_until_the_upgrade_switches`：legacy generation 只用舊語意（筆記與展開都不命中）；`claim_upgrade` 只成功一次；重建卡在 `search.before_switch` 時新寫入照常 sync、搜尋照常回應、沒有 expanded；切換後筆記與展開都命中。
  - `m1_7_http`：經 router：未登入 401、非管理員 POST 403 `forbidden_op`、POST 201、同概念重複 422 `label_exists`、別的概念撞名 409 `concept_label_taken`、hidden 標籤、PATCH 改 staging、非管理員 GET 只看到 2 個標籤、`?q=` 清單、`POST /issues/search` 帶 `match.expanded` 且回應不含 AUTH 的 key。
- `m1_7_upgrade.rs`：0026 → 0027：R18 既有重複（同概念大小寫、跨概念、NFD vs NFC、pref 優先）都能升級，降級為 staging 的就是預期那幾個；概念列與偏好標籤；system principal；非概念 alias 不動；unique index 擋新的啟用撞名、接受 staging；epoch 不變。
- `search.rs` 單元測試：`e1_ndcg_at_10_holds`（E1 60 題、不展開，0.809525，門檻 0.8095）、`e1_with_alias_concepts`（E1 別名表當概念，0.841755 ≥ 0.810）；`filter.rs` 的 `index_prefilter_is_a_superset` 加入 status／priority 與 legacy 改寫後的超集斷言。
- 既有測試的修改：`s7_redaction::search_never_shows_redacted_text` 原本斷言公開筆記搜不到（T-S4 舊行為），M1.7 起公開筆記進索引（§3.9、T-M1-63），改成命中 1 筆；私有筆記仍是 0。
- 沒測到：前端（概念管理畫面、搜尋結果的「透過 X 找到」）未接；BM25F 權重的排序品質只有 E1（沒有筆記、自訂欄位的評估資料）；`main.rs` 的自動升級迴圈沒有整合測試（測試直接呼叫 `claim_upgrade` + `rebuild`）。

## M1.8 疑點與照建議的做法

- M1.8-D1 **權限**：不需要新權限，0028 不動 `role_permissions`、不 bump epoch（升級測試用 `run_to(28)` 驗證）。關注、偏好、通知都是個人狀態，不寫 `issue_events`、不動 `lock_version`。只有人（human session）能用這些端點；agent 呼叫時被 `commands::executor` 擋下，MCP 沒有通知或關注工具（§4.6）。
- M1.8-D2 **收件人重驗**：派送時對每個收件人用他自己的身分（新增 `AuthMethod::Recipient`，`db::effective` 只認 active 的 user）載入可見範圍，逐事件走 `issue_view::event_view`（和 issue 歷史同一套規則：私有 issue、私有筆記、受限欄位、另一端看不到的 duplicate）。只改了讀不到的欄位的事件不送；被遮蔽（D12）的事件不送。讀取（列表、未讀數、標記）時每次再重驗，撤權後既有的列在下次讀取就消失，未讀數跟著降。通知列只存參照（issue、event id、原因），不存文字。
- M1.8-D3 **交易切分（和契約不同）**：契約寫 progress 與 cursor 在同一交易；實作是每張 issue 一個交易（鎖 progress 列 → 讀新事件 → 派送 → 更新 progress），整批處理完才單調前進 cursor。理由：一批 200 筆訊息放在同一個交易會拉長交易、拖住搜尋 GC 的 xmin，且一張 issue 出錯會連帶整批重做。重複投遞由 progress 擋（T-M1-77 驗證：在 `notify.before_cursor` 崩潰、cursor 倒回 0 都不會重送）。
- M1.8-D4 **平行與批次**：一輪內不同 issue 由 `OXLEDGER_NOTIFY_WORKERS` 個 worker（1–16，預設 4，超出範圍用預設並記 warn）平行處理（各自連線與收件人快取；不同 issue 之間沒有共用列）；同一張 issue 的所有收件人一次鎖未讀列、一次 UPDATE、一次 INSERT（jsonb 陣列展開）。原本逐人 2 次往返時 p95 是 12.5 秒，改完 5.53／6.71 秒（兩次量測）。
- M1.8-D5 **時間欄位與 read-all**（獨立驗證 D1 後改）：`first_at`／`last_at` 用派送時間（`clock_timestamp()`），不是事件時間。光用派送時間不夠：平行派送時，先讀時間、後 commit 的列會落在列表的 before 之前而被 read-all 一起標掉。所以派送在讀時間之前先依 key 順序取得每個收件人的 `pg_advisory_xact_lock`（`hashtextextended('notify-recipient:'||id, 0)`），持有到 commit；`read-all` 也先取呼叫者的同一把鎖。結果是同一個人的列依 `last_at` 順序 commit：列表看得到的 last_at ≤ before 的列，就是 read-all 會標的列；之後併入的事件會把 last_at 推過 before，那一列維持未讀。沒用 snapshot／xmin token，因為 before 是時間戳，前端不必改；鎖只在同一個收件人之間序列化，p95 從 5.53 秒變 5.45 秒（誤差內）。
- M1.8-D6 **冪等**：`read`、`unread`、`read-all`、`watch` 天然冪等，不收 `Idempotency-Key`；`PUT /me/notification-preferences` 收 `Idempotency-Key` 並要求 `lock_version`（衝突回 `conflict` 附目前值）。
- M1.8-D7 **存在性 oracle**：對看不到的 issue 呼叫 watch，回應和不存在、格式錯誤的 key 相同（`not_found`，只差訊息裡的 key）；看不到的 project 篩選回空清單；別人的列、看不到的自己的列、不存在的 id 一律 `not_found`；`read-all` 不回筆數；`watcher_count` 只算現在看得到這張單的 active user（上限 100）。**（獨立驗證 D2 後改）** `watcher_count` 改成一句 SQL：照 `db::load_scope` + `Scope::can_see` 的規則（管理員、使用者與其群組的成員角色、公開專案的 non_member 角色、只算未封存且啟用 issue_tracking 的專案、tracker 限制、all／default／own）對單一 issue 判斷，最多看 1000 個關注者。原本每個關注者各做一次 `load_scope`，1000 個看不到的關注者時 GET 要 3.7 秒。`watcher_count_both`（test-support）並排算 SQL 與 Rust 兩種結果，測試比對它們一致。
- M1.8-D8 **列表**：每列最多 3 筆事件（最新在前）加 `more_events`；一列最多保留 50 個事件 id（舊的丟掉）；`(last_at DESC, id)` keyset 分頁，每次最多掃 500 列。被遮蔽的事件在讀取時以 `redacted` 標記顯示，該列保留。
- M1.8-D9 **自動關注**：建立者、新的指派對象（群組不展開）、寫筆記的人；agent 寫筆記時寫給委派者。明確取消關注會留 optout，之後的自動關注不再寫入；手動關注會清掉 optout，並把既有的自動來源改成 `manual`。派送時才讀關注者，所以在派送前加入的關注者也會收到該批事件。**（獨立驗證 D4 後改）** 派送寫自動關注前和 `PUT|DELETE …/watch` 都先取同一把 `pg_advisory_xact_lock`（key 為 issue＋人），持有到 commit：取消關注若在派送寫入後才來，會等派送 commit 再刪；先來的話，派送會看到它留下的 optout。
- M1.8-D10 **指派**：被移除指派的前一位也收到該事件（原因 `assigned`）；指派給群組時展開為 active 成員。
- M1.8-D11 **agent**：agent 永遠不是收件人或關注者（`user_column_guard` 也擋）；agent 的動作給委派者 `agent_on_behalf`（不受 `on_self` 影響，受 `on_my_agents` 控制）；作者是 agent 時改用 `created_on_behalf_of`。**（獨立驗證 D3 後改，待使用者確認）** `on_my_agents = false` 照字面解釋：自己委派的 agent（事件的 `on_behalf_of` 是收件人）做的事一律不通知委派者，就算他是關注者或作者也一樣。契約 §3.11.2 沒寫清楚 on_my_agents 是只關掉 agent_on_behalf 這個原因，還是整個靜音。
- M1.8-D12 **triage_decided**：`triage_changed` 的 new 為 accepted／declined／duplicate 時給作者 `triage_decided`（受 `on_triage_decided` 控制）；snooze 不算。
- M1.8-D13 **偏好與靜音**：第一次 GET 建列；靜音只收看得到的專案（`invalid_value`，kind `unknown_project`，看不到和不存在相同；上限 200 個，`too_many_projects`）；之後看不到的既有靜音保留但不顯示。
- M1.8-D14 **未讀數**：上限 99（`capped`）；2 秒內數不完回上次的值並標 `stale`；權限指紋（admin、群組、可見與所屬專案的 epoch）變了就回 `count: null`。快取在行程記憶體裡（多個 serve 行程各自一份）；最多 10,000 人，滿了遇到新的人就整批清空（獨立驗證 D7）。
- M1.8-D15 **效能量法**：`perf_dispatch_p95`（`#[ignore]`）：50 人、1 萬張單（SQL 一次匯入、無事件）、每張單 5 個關注者；dispatcher 照 `serve` 的方式跑（一輪後睡 1 秒，滿批不睡）；10 人同時改 subject 各 60 次（約 100 事件／秒的尖峰）；延遲＝命令回傳 → 事件出現在任何通知列（每 50 ms 查一次）。資料庫在本機 Docker；正式環境的網路延遲與平常的負載都沒量。
- M1.8-D18 **一直失敗的 issue**（獨立驗證 D5）：同一張 issue 連續失敗 `MAX_ISSUE_FAILURES`（5）次後，記 error log、`notify_skipped_issues_total` 加一，把它的 progress 推到目前最後一個事件（那些事件不送），其他 issue 照常處理，cursor 前進。一輪裡其他 issue 不再因為某一張失敗而停下（失敗的那張讓這一輪不前進 cursor，下一輪重試）。連線類錯誤（I/O、pool 逾時或關閉、TLS、protocol）視為資料庫問題，不計次數，避免資料庫短暫斷線時把所有 issue 都跳過。計數放在行程記憶體裡。**停用開關沒做，待使用者決定。**
- M1.8-D19 **顯示的原因**（獨立驗證 D6）：0028 加 `event_reasons`（和 `event_ids` 平行，每個事件的原因以逗號串接），讀取時 `reasons` 只取讀者現在還讀得到的事件的原因聯集。`first_at`／`last_at` 不重算：read-all 的 before 以 DB 的 last_at 為準，前端拿列表第一列的 last_at 當 before，重算會讓兩者不一致；代價是已隱藏事件的到達時間仍反映在列的時間上。
- M1.8-D16 **ai_budget**：表與 CHECK 先留好，M1.8 不產生也不計數（M1.10）。
- M1.8-D17 **保留期**：已讀 30 天、未讀 90 天（以 `last_at` 算），`serve` 每小時清一次，每批 5000 列。

### 測試

- `m1_8_notifications.rs`（22 個＋1 個 ignored）：
  - `check_lists_match_constants`：`issue_watchers.source`、`notifications.kind`、`notifications.reasons` 的 CHECK（`pg_get_constraintdef`）與 `WATCH_SOURCES`、`KINDS`、`REASONS` 相同。
  - `delivery_basics`：作者自動關注、`on_self` 預設關、關注／指派原因、同一張單的事件併入未讀列、筆記摘錄、issue 詳細的 `watch`、讀過後下一個事件開新列。
  - `t_m1_70_not_found_is_uniform`、`t_m1_71_private_note`、`t_m1_72_restricted_field`、`t_m1_73_private_issue`、`t_m1_74_75_revocation`（含遮蔽標記、重新加入後可讀）、`t_m1_76_agents`、`t_m1_77_at_least_once_is_exactly_once`、`t_m1_78_auto_watch`、`t_m1_79_self_mute_locked`、`t_m1_80_132_unread_count`、`t_m1_132_watcher_count`、`t_m1_81_read_all_before`、`t_m1_82_gc_waits_for_notify`（含 `/metrics` 的 `notify_outbox_pending`）、`t_m1_83_no_mcp_tools`、`t_m1_125_user_guard`（agent 與群組寫入五張表都被 23514 擋，UPDATE 也擋）、`t_m1_126_read_race_and_unread_merge`（在 `notify.deliver` 卡住時標已讀 → 新事件進新列；`unread` 合併）。
  - `triage_decided_reaches_the_reporter`、`retention_purge`、`paging_and_event_cap`（cursor 分頁、52 個事件時 `more_events`）、`m1_8_http`（9 個路由經 router；watch 的 404 本文和不存在相同；prefs PUT 沒有 Idempotency-Key 是 400、重送回相同結果、未知欄位 400）。
  - `perf_dispatch_p95`（ignored）：見 M1.8-D15。
- 獨立驗證後追加（`m1_8_notifications.rs` 10 個）：`v_read_all_before_marks_row_committed_after_list`（D1：x 的派送卡在自動關注、y 的派送卡在 progress 列；放開 y 後 y 等 alice 的鎖，列表一直是空的；x 回來後兩列依 commit 順序出現；之後 x 有新事件，read-all 只標 y）、`v_watcher_count_with_many_invisible_watchers`（D2：1000 個看不到的關注者，GET < 500 ms）、`v_watcher_count_matches_scope`（D2：管理員、群組指派、default／own、tracker 限制、公開專案 non_member、鎖定、封存專案，SQL 與 load_scope 結果一致）、`v_on_my_agents_off_also_for_watching`（D3）、`v_unwatch_during_auto_watch`（D4：派送在 `notify.after_auto_watch` 卡住時取消關注要等，最後 optout 成立）、`v_poison_issue_is_skipped`（D5：`notify.issue:<id>` 連續 5 次失敗，第 5 次跳過、其他 issue 第一輪就送出、metric 增加、之後的新事件照送）、`v_reasons_follow_visible_events`（D6）、`v_redacted_before_dispatch_not_sent`、`v_progress_lock_with_read_between`、`v_revocation_variants`（欄位改受限、issue 改私有、移到沒加入的專案、事件層級遮蔽（列保留、只有標記）、角色失去 view_issues）。mutation：拿掉派送的收件人鎖 → D1 測試失敗；拿掉 watch 的鎖 → D4 測試失敗（都已還原）。`t_m1_125_user_guard` 補上錯誤訊息斷言（確認是 guard 擋的，不是其他 CHECK）。
- `m1_8_upgrade.rs`：0027 → 0028：epoch 不變、`notify` cursor 為 active、有 30 個事件的舊單 progress 回填為 30；升級後 cursor 倒回 0 仍只送第 31 個事件。
- 沒測到：前端（鈴鐺、通知中心、關注按鈕、偏好畫面）未接；`main.rs` 的派送與清除迴圈沒有整合測試（測試直接呼叫 `dispatch_round`／`dispatch_all`／`purge`）；多個 serve 行程同時派送只靠 progress 列鎖，沒有專門測試。

## M1.9 疑點與照建議的做法

- M1.9-D1 **權限與 epoch**：0029 不動 `role_permissions`、角色或成員，不 bump `security_epoch`（`m1_9_upgrade` 用 `run_to(28)`→`run_to(29)` 驗證）。AI 團隊頁、暫停、退役都用既有的身分判斷（贊助人、站台管理員、該專案有 `manage_project` 的人），沒有新權限。
- M1.9-D2 **契約之外多加的欄位**：`agent_runtimes.lock_version`（改名的樂觀鎖）、`agent_principals.retired_at`（`agent_principals` 沒有 status 欄，退役要有地方記）、`changesets.chain_review`（鏈上限送審的提案不能勾選核准、待審佇列顯示原因）。另加兩個 trigger：`delegations_fill_runtime`（舊程式路徑沒帶 runtime_id 時自動補）、`agent_credentials_freeze`（token 的 context、principal、runtime 不可改）。
- M1.9-D3 **舊路徑相容**：`POST /delegations` 帶 `agent_client_id`（02 的寫法）照舊可用，落在該 client 的 `personal` 角色（沒有就自動建 runtime 與角色）；帶 `agent_principal_id` 則落在指定角色。兩者恰好一個，否則 `invalid_request`。`/agent-contexts*` 保留為別名；當 runtime 只有一個角色時 `POST /agent-contexts/{id}/credentials` 不帶 `agent_principal_id` 也行，有多個角色時回 `invalid_argument`（`details.reason = "principal_required"`，照契約 §5；獨立驗證 D6 前是 `details.kind`）。舊路徑新建角色時一樣受 20 個角色上限（`role_limit`，`path = agent_client_id`；獨立驗證 D4）。
- M1.9-D4 **暫停**：暫停不要 step-up（要能立刻止血），恢復要 step-up。principal 層：贊助人或站台管理員；站台管理員的暫停只有站台管理員能恢復，贊助人的暫停贊助人或站台管理員能恢復。專案層：該專案有 `manage_project` 的人或贊助人，理由必填（`required_field_missing`），只有同一層能恢復。暫停擋所有寫入路徑（直接寫、提案、核准後套用）；`validate` 不擋（只是預覽）。恢復時重驗脈絡、隔離狀態、斷路器（脈絡被 02 §16.6 停用時回 `agent_suspended`，要先恢復脈絡；獨立驗證 D9）與至少一個可用的委派。
  - **（獨立驗證 D2）** 寫入檢查暫停時用 `FOR SHARE` 讀 `agent_principals` 與該委派列，持有到寫入 commit：暫停會等進行中的寫入結束，暫停 commit 之後開始的寫入一定被擋。寫入之間不互擋（共用鎖）。測試 `v_pause_waits_for_running_write`（fault point `ai_team.after_pause_check`；拿掉 `FOR SHARE` 時測試失敗，已還原）。
  - **（獨立驗證 D3）** 站台管理員暫停角色時，同時在該 runtime 記 `admin_paused_*`（0029 新欄位）。trigger `agent_principals_inherit_admin_pause`：同一個贊助人之後新建的角色，只要在這個 runtime，或 client 相同（含已退役的 runtime 裡的角色），一建立就是「站台管理員暫停」，只有站台管理員能解除。所以退役角色或整個 runtime 再重開都繞不過。站台管理員恢復時，一併清掉這些 runtime 上的標記（同 runtime 還有別的角色被站台管理員暫停時保留）。
  - **（獨立驗證 D3）** 誰暫停的：詳細頁的 `pause.paused_by`（`sponsor`／`site_admin`，只給贊助人與站台管理員）；專案層在該專案的政策列 `paused_by`（`sponsor`／`site_admin`／`project`，只給看得到該政策列的人：贊助人、站台管理員、該專案管理者）。`pause.scope` 現在固定是 `principal`。專案層暫停可由贊助人解除（契約允許），不改。
- M1.9-D5 **鏈上限**：判斷順序照 §3.12.4：結構驗證 → 政策（`off` 一律 `forbidden_op`）→ 鏈（只有 `direct` 會被改成送審）→ 配額。鏈檢查前用 `FOR UPDATE` 鎖相關 issue 列。**（獨立驗證 M14 的分析）** 這把鎖是第二道防線，不是計數正確的原因：鏈計數的 issue 一定是文件的既有 target，每個既有 target 都要帶 `lock_version` 前置條件，每次有效寫入都讓 `lock_version` +1（02 K8），執行器在 `FOR UPDATE` 的列上比對。所以兩個同時寫同一張單的直接寫入最多只有一個 commit，沒有這把鎖也不會超過上限。這把鎖只讓後到的那個等在這裡、看到新的計數（得到 `agent_chain` 送審而不是 `conflict`），並在前置條件哪天變成選填時仍然成立。因為拿掉它也沒有測試會失敗，所以沒有另寫確定性測試，理由寫在 `chain_exceeded` 的註解。`v_chain_concurrent_at_limit` 驗證 19 次後兩個並行寫入：鏈停在 20，另一個送審。`validate` 也做鏈檢查（同樣上鎖，交易最後 rollback）。核准後套用的事件 origin 是 `external_agent`，由 `issue_events` trigger 清掉鏈（P4：人核准就是人介入）。
- M1.9-D6 **qa 範本的 transition 白名單**：契約要 qa 只能直接做白名單內的狀態轉換，但沒有存白名單的地方。**目前 qa 的 transition 一律 `review`（比契約窄）**，待使用者決定白名單放哪裡。
- M1.9-D7 **pm_assistant 的 subject／description 是 `review`**：欄位政策對 `update_fields` 一律適用，所以 pm 改標題也送審。**待使用者確認**（和 D6 一起）。
- M1.9-D8 **範本與請求的政策**：範本寫成完整的 allow-list（沒列的 op 與欄位寫 `off` 列）；`POST /ai-team` 的 `delegations[].policies` 可以覆寫範本（贊助人自己的選擇；執行時一律再和贊助人自己的權限取交集，所以不會超過贊助人能做的事）。`builtin_chat` 不能由外部建立。
- M1.9-D9 **`POST /ai-team`**：至少一個委派；一個贊助人最多 20 個 active 角色（`invalid_value`，kind `role_limit`）；`Idempotency-Key` 必填，重送回相同 id、`token: null`（token 只出現一次）。共用 runtime 時回 `warnings[]`（kind `shared_runtime`，列出其他角色、R 的來源專案與 `time_readers`）。
- M1.9-D10 **AI 團隊列表**（獨立驗證 D8 後改）：
  - 看得到的角色＝自己是贊助人，或在「自己是成員（本人或所屬群組）且讀得到」的專案有未撤銷（含過期）的委派。
  - 公開專案的非成員（只靠 non_member 角色讀）不算：契約 §3.12.5 寫「看得到該專案的成員」。
  - 站台管理員看得到所有外部角色，包含沒有委派的（§3.12.1 事故處理）。
  - 內建 AI（builtin runtime）在列表與詳細頁都排除（契約沒寫，M1.10 再定）。贊助人對自己的內建 AI 發 token 仍回 `builtin_no_token`。
  - 非贊助人看不到 `duties`、`blocked_7d`。
  - 看不到的角色在詳細頁回 `not_found`，和不存在、已退役完全相同（HTTP 層 `v_http_invisible_equals_missing` 比對狀態、本文、標頭）。
  - `next_cursor` 目前一律 null（上限 20 個角色 × 贊助人，不分頁）。
- M1.9-D14 **活動與統計逐筆過濾**（獨立驗證 D1）：
  - 每筆 execution 逐個事件用 issue 歷史同一個函式（`issue_view::event_view`）判斷。讀不到的私人備註、受限欄位（M1.5 `visible`／欄位角色，`readable_cf_ids`）、另一端看不到的 duplicate 都去掉。
  - diff 的 `cf:<id>` 欄位依觀看者可讀的欄位過濾。
  - 過濾後什麼都不剩的 execution 整筆不顯示，也不計入統計（`writes_today`、`undo_rate_7d`）。
  - 02 §16.10 的贊助人活動頁用同一個函式，一起變嚴。
- M1.9-D15 **02 留下的 `oxledger-ai@…` 外觀 handle**（獨立驗證 D5）：0029 不改使用者資料，只 `RAISE NOTICE` 印出數量；upgrade-notes 附查詢 SQL。0029 之後新建的外部角色不能用這種 handle（trigger 擋）。**升級時要不要自動改名，待使用者決定。**
- M1.9-D11 **`describe agent_context`**（獨立驗證 D7 後改）：
  - 只列這個角色自己被委派過的專案。
  - 同一 runtime 其他角色帶進 R 的專案只給數量 `other_role_projects`（0 時不出現，02 的 `scope: context` 輸出不變），不回 key 與 id。這些專案仍算進受眾判斷。
  - 契約 §4.1 只說「02 §5.3 的工作脈絡內容」，沒有要求逐一列出其他角色的專案，所以照建議改。
  - `context_unavailable`（quarantined）的 details 多 `shared_with`（同 runtime 其他角色的 handle），讓 agent 知道不是只有自己停。
- M1.9-D12 **`carry_over_delegations`** 先拒絕（`invalid_argument`），M1.10 再做。
- M1.9-D13 **一個 client 一個 runtime**（Q2）：由 trigger 保證（併發時一方 `runtime_client_taken`）；runtime 退役後 client 釋出。

### 測試

- `m1_9_ai_team.rs`（32 個）：
  - 實作時寫的 13 個：`t_m1_85_88_shared_context`（85、86、87、88、66a 代表子集，含帶 evidence 的情況）、`t_m1_89_runtime_new_context`、`t_m1_90_pause`、`t_m1_91_92_team_page`、`t_m1_93_95_98_123_chain`、`t_m1_94_chain_writes`、`t_m1_96_97_chain_reads_and_concurrency`、`t_m1_99_templates`、`t_m1_121_one_runtime_per_client`、`t_m1_124_check_lists_match_code`、`t_m1_127_builtin`（另驗內建 AI 不在列表與詳細頁）、`t_m1_128_delegations_by_principal_and_alias`、`t_m1_133_time_readers`。
  - 併入獨立驗證的 13 個：`v_activity_hides_private_note_writes`、`v_chain_ignores_log_time`（T-M1-96 的 `log_time` 部分）、`v_chain_concurrent_at_limit`、`v_q2_trigger_concurrent_sql`、`v_chain_review_not_queue_approvable`、`v_chain_review_tickable_shape_not_queue_approvable`、`v_agent_cannot_write_as_human`、`v_template_cannot_exceed_sponsor`、`v_alias_lands_on_personal`、`v_resume_needs_step_up`、`v_pause_reason_not_leaked`、`v_http_invisible_equals_missing`、`v_shared_warning_is_own`。
  - 為驗證缺陷新加的 6 個：`v_activity_hides_restricted_field_writes`（D1 受眾欄位）、`v_pause_waits_for_running_write`（D2）、`v_admin_pause_not_bypassed_by_retire`（D3）、`v_alias_respects_role_limit`（D4）、`v_resume_rechecks_breaker`（D9）、`v_team_visibility_admin_and_public_non_member`（D8）。
- `m1_9_upgrade.rs`（2 個）：
  - `t_m1_84_upgrade_ai_team`：0028 → 0029，V2 四種情況（只有關閉的脈絡、沒有脈絡、client 名稱 100 字、前 56 字相同的標籤衝突），epoch 不變，舊 token 照常認證且範圍相同。
  - `v_upgrade_with_suspension_and_lookalike_handle`：被斷路器停用的脈絡與兩份委派升級後照舊；02 的 `oxledger-ai@u` handle 保留不改，upgrade-notes 的查詢找得到它（D5）。
- T-M1-66a 帶 evidence（獨立驗證 D10）：引用另一個角色讀不到的 issue 時，第 7 步 evidence 檢查先回 `source_unverifiable`，到不了受眾檢查。這是設計：evidence 必須在該 agent 自己的讀取範圍內。測試改用兩個 agent 都讀得到的 WEB issue 描述當 evidence，第 7 步通過、第 8 步受眾檢查回 `audience_expansion_forbidden`，兩者相同。
- 沒測到：
  - T-M1-66a 只測代表子集（筆記、欄位、建單三種公開寫入，以及帶 evidence 的筆記），不是 02 E4w 全集。
  - 升級測試沒有帶未套用的提案。
  - 前端未接。

## M1.10 疑點與照建議的做法

- M1.10-D1 **權限與 epoch**：0030 不動角色、權限、成員，不新增或改狀態任何 principal，不 bump `security_epoch`（`m1_10_upgrade` 用 `run_to(29)`→`run_to(30)` 驗證）。內建 AI 沒有新權限：它的範圍＝使用者的委派 ∩ 使用者自己的權限，和外部 agent 走同一套檢查。
- M1.10-D2 **`AuthMethod::BuiltinRun`**：只能由 `runner` 用 `AuthContext::builtin_run` 建；HTTP（cookie、Bearer）與 MCP 都建不出來。`db::effective` 對它另外要求：run 是 `running`、租約有效、沒有 `cancel_requested_at`、對話開著沒刪、脈絡 active、principal 沒暫停或退役、client 啟用且是 builtin、`ai_settings.enabled`。委派交集、受眾、鏈上限、暫停、配額照常。
- M1.10-D3 **固定清單 CHECK**：`changesets` 的 0009 `CHECK (origin = 'external_agent')` 依定義找到後刪掉，改成具名的 `changesets_origin_agent_ck`（`external_agent`、`builtin_ai`，是既有值的聯集）；新表的清單與 `ai::RUN_STATES` 等常數由 `fixed_lists_match_the_program` 比對。
- M1.10-D4 **0030 與既有的 `oxledger-ai` client**：02 的一般 client 叫 `oxledger-ai` 時 0030 中止並指向 upgrade-notes.md（改名後再跑即可，測試驗證）；已經是 builtin 的（只有 M1.9 測試資料庫）直接接手。測試發現：sqlx 的 migration 失敗後，advisory lock 可能留在連線池的某條連線上，直到它閒置逾時（約 10 分鐘）才釋放；同一個連線池立刻重跑會等那麼久。伺服器遇到 migration 失敗本來就結束程序，不受影響；測試改成重開連線池再跑。
- M1.10-D5 **`user_column_guard`** 也裝在 `ai_runs.user_id`、`ai_usage_daily.user_id`（契約只寫 `ai_conversations.user_id`）。
- M1.10-D6 **使用者畫面的重驗**：讀對話時用「使用者讀得到的每個專案、agent 會有的範圍」重驗（私有 issue、私有備註、受限欄位依 agent 規則遮掉，使用者自己畫面也一樣），不是用目前的委派。撤銷委派只讓送給模型的歷史變成佔位；使用者仍讀得到的內容在畫面上照常顯示。
- M1.10-D7 **把 issue 改私有、移出專案會隔離脈絡**：這兩種變動都會 bump 該專案的 epoch（既有 trigger），內建 AI 的脈絡因此 quarantined，下一次送出回 `context_unavailable`；使用者要「換新脈絡（帶委派）」後開新對話，舊對話唯讀。T-M1-105 的「再送下一則」因此在 run 層用「撤銷委派」驗證歷史佔位，在畫面層用「改私有」驗證。
- M1.10-D8 **輸入上限**：每次請求前檢查累計輸入（provider 回報或估算）是否已達 50 000；不把下一個請求以每字元 1 token 預先加進去（工具 schema 就上萬字元，會讓 run 在第二次呼叫停下）。測試時發現並修正。單次請求上限與結算見 M1.10-D14。
- M1.10-D9 **錯誤對外的樣子**：provider 錯誤只回 `{code: ai_provider_error, kind: timeout|http_status|malformed, status?}`；位址被擋回 `ai_unavailable`（`endpoint_blocked`）；沒有 provider 原文、key、URL 或位址。「測試連線」只能測已登記的端點，回 `connect`／`timeout`／`http_status`／`malformed`／`endpoint_blocked`／`not_configured`，不會變成掃描內網的工具。
- M1.10-D10 **斷路器**：同一 provider＋model 連續 5 次失敗，或 60 秒內 ≥20 次呼叫且失敗 ≥50%，開 60 秒（01 §16.3）；開著時送出回 `ai_unavailable`（`circuit_open`）。位址被擋與沒有傳輸層不計入。只存在記憶體，多程序各自計算。
- M1.10-D11 **沒有 feature `ai` 的建置**：API、管理頁、對話都在；run 立即以 `ai_unavailable`（`not_configured`）結束，串流只回 `resync`。`/admin/ai/endpoints` 的 `transport` 告訴前端。
- M1.10-D12 **其他預設**（詳見 decisions.tsv）：退役使用中的 provider 會關掉 AI；Anthropic 沒 key 顯示為 `endpoint_unregistered`；解析不到的端點要勾 org_account_attested；設定 PUT 只有啟用與換 provider 要 step-up；歷史只在 run 開始時重驗；月預算 100% 在每輪工具後檢查；沒有本程序 hub 的取消直接結束（用 run 已記錄的用量結算，見 D14）；委派快到期只放在 status；保存期限每小時清；第三條串流 `rate_limited`；延長＝撤銷（reason `extended`）再建。
- M1.10-D13 **待使用者決定**：
  - 斷路器狀態只在記憶體（多程序部署各自計算）；要不要存 DB。
  - 內建 AI 讀頁面脈絡時，issue 帶 `recent_notes`（依 agent 規則過濾）；要不要只帶欄位。
  - provider 文件與實作的差異見 `m1-10-provider-notes.md`（OpenAI 的 `max_tokens` 已棄用，實作仍送 `max_tokens`；Anthropic `refusal`、`pause_turn` 一律當作這一輪結束）。
- M1.10-D14 **獨立驗證後的修正**：
  - D1：頁面脈絡的來源專案也併進該 run 回覆的 `source_projects`；D12 另外把「任何訊息引用該專案的 run」的所有 assistant 訊息清空；`chat::unreadable` 把已清空（parts 為 NULL）的 tool 訊息視為讀不到，整個 run 的回覆變佔位。
  - D2（Q4）：`ai_conversations.last_provider_config_at` 記住對話上次用的 provider 設定版本（`config_changed_at`）；同一個 provider 但端點或模型改過，也要確認 `ai_provider_changed`。改使用中 provider 的端點或模型會把 AI 關掉（事件 `settings_updated`、`reason: provider_config_changed`），要重新測試連線再啟用。
  - D3：單次請求輸入估算：ASCII 字元算 ¼ token、其他字元（中文等）算 1 token，上限 16 000（每字元 1 token 會讓系統提示加工具 schema 本身就超過 16k，所有帶工具的 run 都送不出去；測試時發現並改）；超過時頁面脈絡與工具結果從最舊的開始截短（尾端加 `[… cut: over the request input limit]`，使用者的文字與工具呼叫不截）；仍放不下就不送，run 以 `budget_exhausted` 結束，`run.finished` 帶 `error.kind = request_input_limit`（沿用契約固定的 stop_reason 清單，不新增）。結算：provider 回報的用量照實記錄（可超過保留額，月預算照實累計並觸發 100% 通知）；只有估算用量以保留額為上限（契約 V15）。
  - D4：沒有本程序 hub 的取消，用 DB 上 run 已記錄的 `input_tokens`／`output_tokens`／`usage_estimated` 結算（同清掃）；執行中的 run 每輪更新用量時限 `state IN ('queued','running')`，已被別處結束的 run 不再被改寫。
  - D5：NAT64 `64:ff9b::/96`、IPv4-compatible `::/96`、IPv4-translated `::ffff:0:0:0/96`、6to4 `2002::/16` 依內含的 IPv4 判斷，且只有公網能過（內含 loopback／私有也擋：本機 provider 應直接登記自己的位址）；`fec0::/10`、NAT64 local-use `64:ff9b:1::/48`、Teredo `2001::/32` 一律擋。
  - D6：`db::effective` 的 BuiltinRun 在可寫交易中 `FOR SHARE OF r`（取消、清掃、run 結束要等寫入交易提交）；READ ONLY 交易不能鎖列，讀取路徑不加。租約 30 秒、每 10 秒續期、每 15 秒清掃：程序死掉後最慢 30＋15＝45 秒結束（≤ 60 秒），單元測試 `a_dead_run_ends_within_a_minute` 檢查。
  - 工具呼叫前的測試注入點 `ai.before_tool`（只在 feature `test-support`），讓測試在 runner 自己的檢查之後改 DB，驗證工具呼叫本身的憑證重驗。
  - 已知（不改）：`time_list` 的來源是脈絡的所有委派專案，撤銷任一個委派都會讓歷史裡的工時結果變佔位。

### 測試

- `m1_10_ai.rs`（13 個，不需要 feature）：`t_m1_115_defaults_and_no_delegation`、`t_m1_112_113_admin_providers_and_keys`、`t_m1_114_119_endpoint_addresses`、`t_m1_108_conversations_are_private`、`t_m1_101_page_context_takes_references_only`、`t_m1_107_delete_and_retention`、`t_m1_105_106_recheck_and_d12`、`t_m1_116_new_context_carries_delegations`、`t_m1_134_extend_delegations`、`t_m1_109_rate_and_budget_refusals`、`t_m1_131_provider_change_needs_ack`、`fixed_lists_match_the_program`、`t_m1_125_conversation_user_guard`。
- `m1_10_ai_run.rs`（22 個，feature `ai`；測試內的假 provider 兩種協定、假 DNS、攻擊者伺服器）：`t_m1_117_both_protocols_same_events`（含 T-M1-131 歷史送到 B）、`t_m1_115_118_no_tools`、`t_m1_100_105_canaries_never_reach_the_model`、`t_m1_102_103_writes_follow_the_policies`、`t_m1_102_audience_and_injection`（含 T-M1-104）、`t_m1_122_time_taint`、`t_m1_109_budget_reservations_and_notifications`（10 人搶最後 0.30 USD）、`t_m1_110_stream_reconnect_resync_cancel`、`t_m1_111_119_120_provider_errors_and_breaker`、`t_m1_119_dns_rebinding`、`t_m1_129_lease_sweep`；獨立驗證後加的 12 個：`iv1_d12_clears_a_reply_from_page_context`、`iv2_q4_endpoint_change_in_place`、`iv3_request_input_is_capped`、`iv3b_a_request_that_cannot_fit_is_not_sent`、`iv4_cancel_without_local_hub_keeps_usage`、`iv5_ip_spellings_are_normalised`、`iv6_time_taint_cleared_by_placeholder_whole_run`、`iv7_other_time_and_restricted_field_canaries`、`iv8_not_found_is_identical`、`iv_m6_m8_m10_each_tool_call_rechecks_the_run`（取消、租約過期、run 不在 running、AI 關閉、client 停用、client 不是 builtin，各自在工具呼叫前改 DB，驗證回 `unauthenticated` 且沒寫入）、`iv_m9_tools_not_offered_never_run`。
- `m1_10_upgrade.rs`（2 個）：`upgrade_0030_moves_no_epoch`、`upgrade_0030_refuses_an_ordinary_client_named_oxledger_ai`。
- 程式內單元測試：SSE 解析、兩種協定的編解碼、端點 URL 解析、位址分類（含 `ipv6_embedded_ipv4_forms`）、歷史重組、斷路器、工具摘要、租約時間（`a_dead_run_ends_within_a_minute`）。
- 變異測試（手動暫改程式後確認新測試失敗，再還原）：M6（不查 `cancel_requested_at`）、M8（不查 `cl.enabled`／`cl.builtin`／`s.enabled`）、M10（不查 `state = 'running'`／租約）、M9（沒提供的工具照樣執行）。
- 沒測到：
  - T-M1-129 用「把租約改成過期再清掃」模擬程序死掉，沒有真的殺程序。
  - T-M1-130 是前端元件測試，後端不做。
  - key 不進日誌：只檢查了錯誤、回應、DB 與 `Debug` 輸出，沒有攔截 tracing 輸出。
  - 真的 provider 沒有跑（見 `m1-10-manual-check.md`）。前端未接。

## M1.11 發布門檻

### 命令 × 身分矩陣（T-M1-66）

命令（列）：
- `add_note`：02 的對照組。
- `update_fields`（`estimated_hours`，M1.0）。
- `triage_decide accept`（M1.3）。
- `log_time`、`update_time_entry`、`delete_time_entry`（M1.4）。

每一格都在全新的目標上執行，並比對寫入前後的領域指紋。

| 身分 | note | estimate | triage | log | update time | delete time |
|---|---|---|---|---|---|---|
| 人，有權限（alice） | applied | applied | applied | applied | applied | applied |
| 人，沒權限（erin，Viewer） | forbidden_op | forbidden_op | forbidden_op | forbidden_op | not_found | not_found |
| 人，看不到（frank） | not_found | not_found | not_found | not_found | not_found | not_found |
| agent direct | applied | applied | applied | applied | applied | invalid_request |
| agent review | review:policy | review:policy | review:policy | review:policy | review:policy | invalid_request |
| agent off | forbidden_op | forbidden_op | forbidden_op | forbidden_op | forbidden_op | invalid_request |
| agent 脈絡失效（STL 受眾變動） | context_unavailable | context_unavailable | context_unavailable | context_unavailable | context_unavailable | context_unavailable |
| agent principal 暫停 | agent_paused | agent_paused | agent_paused | agent_paused | agent_paused | invalid_request |
| agent 專案暫停 | agent_paused | agent_paused | agent_paused | agent_paused | agent_paused | invalid_request |
| 人，讀全部工時、只有 `edit_own_time_entries` | forbidden_op | forbidden_op | forbidden_op | applied | forbidden_op（permission） | forbidden_op（permission） |
| 人，只讀自己工時、只有 `edit_own_time_entries` | forbidden_op | forbidden_op | forbidden_op | applied | not_found | not_found |
| 同 runtime 的角色（R 含 AUTH） | audience_expansion_forbidden | audience_expansion_forbidden | audience_expansion_forbidden | applied | applied | invalid_request |
| 不同 runtime 的角色 | applied | applied | applied | applied | applied | invalid_request |
| 內建 AI direct／review／off | 同外部 agent | | | | | |
| 內建 AI principal 暫停 | run 以 `ai_unavailable` 結束（沒有工具執行） | | | | | |
| 內建 AI 專案暫停 | agent_paused ×5 | | | | | invalid_request |
| 內建 AI 脈絡失效 | 送出被拒 `context_unavailable` | | | | | |

- 角色 bot 與 oracle 比對（T-M1-66a）：
  - 同 runtime 的 bot 要和「一個 agent 同時委派 WEB＋AUTH」的結果完全相同。
  - 不同 runtime 的 bot 要和 direct 完全相同。
- 每個 `not_found` 都另外送一份「目標不存在」的同一份文件，兩個回應去掉 id 後要完全相同。
- HTTP：
  - OpenAPI 的每一條非 GET 路由：帶 agent token 一律 401；不帶憑證是 401，或 400 `invalid_body`（JSON 解析先於驗證）。都沒有寫入。
  - 約 44 條 M1 路由（視圖、triage、工時、表單與欄位、首頁、概念、通知、AI 團隊、AI 對話與管理），由沒有權限的已登入使用者用真實 id 與不存在 id 各打一次：兩個回應完全相同，都是 404 或 403，沒有寫入。另外檢查涵蓋：OpenAPI 的每一條寫入路由都要在表裡，或在「只動自己的東西」「前面階段已測」清單裡（D6）。

### 各項 = 0 的證據

| 項目 | 測試 |
|---|---|
| 越權 | `t_m1_66_command_identity_matrix` 的 `overreach == 0`；兩個 HTTP 測試；`t_m1_66_builtin_ai_column`；`t_m1_67_agents_cannot_name_people_or_decisions`（13 案例全拒） |
| 繞過政策 | 矩陣的 review／off／暫停列；`t_m1_69_prompt_injection_external_agent`、`t_m1_69_prompt_injection_builtin_ai` |
| 重複效果 | `t_m1_68_idempotency_of_new_commands`（estimate、triage、log、update time 各重送：同一結果、效果只有一次） |
| 被拒仍有效果 | 矩陣的 `refused_effects == 0`；內建 AI 欄同樣檢查 |
| canary 洩漏 | `canary_notifications`（通知列表、未讀數、已讀／未讀、派送；沒有 SSE 端點）；`canary_ai_everywhere`（模型請求、`ai_messages`、`chat::messages`、run 的 SSE 與檢視；看不到的與不存在的工具結果相同） |
| key 送往非登記主機 | `t_m1_120_key_only_to_its_registered_host`（攻擊者伺服器收到 0 個帶 key 的請求，所有帶 key 的請求都送到登記主機）；`t_m1_119_names_and_addresses`；另見 M1.10 的 `t_m1_119_dns_rebinding`、`t_m1_111_119_120_provider_errors_and_breaker` |
| 內建 AI 讀工時後的 direct 寫入 | `t_m1_122_no_direct_write_after_reading_time`（5 種寫入在 `time_list` 之後都是 `review:time_taint`，同對話的下一個 run 也一樣；計數 0） |

### 疑點與照預設的做法

- M1.11-D1 **修正：工時存在性 oracle**：`log_time`，或 `update_time_entry` 改單，指向看不到的單時，原本回「沒有專案」，和不存在的單不同。現在先檢查單的可見性，回同一個 `not_found`（`time_entries.rs`）。
- M1.11-D2 **修正：`POST /admin/ai/providers` 先查管理員**：原本非管理員會先拿到 422 `ai_endpoint_invalid`，可以藉此探測端點登記，也會觸發 DNS 查詢。現在一律 403（`ai/admin.rs`）。
- M1.11-D3 **站台層表單**：非管理員對真實的站台表單回 403，對不存在的回 404。站台表單不屬於任何專案，它的存在不算祕密，照預設接受；存在性比對改用專案層表單（AUTH），那邊一律 404。
- M1.11-D4 **匿名請求送錯 body** 回 400 `invalid_body`，不是 401：axum 的 Json extractor 先於驗證執行，只會洩漏公開的 schema。照預設接受。
- M1.11-D5 **內建 AI 的 principal 暫停**：run 直接以 `ai_unavailable` 結束，沒有工具結果，所以不是逐筆的 `agent_paused`。效果相同（0 寫入）。
- M1.11-D6 **provider 的 `model`／`name`** 可以填任意字串（包括 URL），但不影響請求送往哪裡。T-M1-120 把這兩個欄位視為無害。
- M1.11-D7 **E5 判定**：每次 run 都在全新的世界上跑；跑完模擬使用者核准所有待審提案，再看資料狀態。唯讀案例另外看回答文字。外部 agent 由測試裡的迴圈呼叫模型，工具走 MCP。詳見 `m1-11-e5.md`。
- M1.11-D8 **待使用者決定**：
  - `agent_principals` 的 handle trigger 用 `LIKE 'oxledger-ai@%'`，大小寫有別，`OXLEDGER-AI@…` 擋不到。API 只產生小寫 slug，所以目前做不出這種 handle；要從 DB 層補，需要新的 migration（改用 `lower(handle)`），本階段依規則沒有做。
  - 要用真模型跑 E5（`e5_live`），需要使用者提供：公司帳號的 Anthropic key（`claude-sonnet-5-5`），以及一個自架 OpenAI 相容端點。

### 獨立驗證後的修正（2026-11-11）

- D1 **`log_time` 帶 `project_id`**：`project_id` 和單的專案不一致時，原本先於單的可見性回錯誤。現在先記下，確認單看得到才回（`time_entries.rs`）。
- D2 **agent 提案的 destination**：`destination` 帶 agent 的寫入範圍（`db::write_scope`：委派者的範圍切到這個 context 曾經委派過的所有專案，含已撤銷、已過期；沒有私人單）。從沒委派過的專案的單、看不到的單回 `not_found`，和不存在相同，再判斷 `cross_project`。曾經委派、後來撤銷的專案照 02 T-X19 回 `delegation_unavailable`（`s5_changesets`、`s6_audience` 要求；只用有效委派會讓這兩個 02 測試失敗）。這表示對已撤銷的專案，agent 仍能分辨「單存在」與「不存在」；它在撤銷前本來就讀得到這些單，照預設接受。工時命令也只算看得到的專案（`cmd_projects_seen`）。擁有者修訂路徑不帶範圍，受眾規則（M9）不變（`changesets.rs`）。
- D3 **缺 precondition 的 oracle（02 就有）**：新增 `commands::parse_for`，帶呼叫者的可見範圍；已存在但看不到的目標先回 `not_found`（用呼叫者自己寫的 key 或 id），再檢查 precondition。`run`、`check_as`（validate）、表單驗證、agent 送出與提案都改用它。`parse` 不帶範圍，給已經查過權限的路徑用（`commands.rs`、`forms.rs`）。
  - 重送同一個 Idempotency-Key 的同一份請求（指紋相同）仍然 replay，不受這個範圍限制：02 的 M05（單變成看不到後重送，回 0 個效果）要維持（`s4_commands` 驗證）。做法是 `commands::resent`：`parse_for` 拒絕後，只有在這個 key 已存在而且指紋相同時，才改用不帶範圍的解析；用過的 key 搭配新文件，仍然回範圍的答案。人與 agent（`agent_write_tx`）都一樣。`t_m1_66_reused_key_is_no_existence_oracle` 驗證；拿掉指紋比對的 mutation 會讓它失敗。
- D4 **矩陣多兩列**：讀全部工時但只有 `edit_own_time_entries` 的人（改別人的工時 → `forbidden_op/permission`），以及只讀自己工時的人（→ 和不存在相同）。兩人也是 AUTH 成員（只有 view_issues），否則受眾規則會讓寬委派 oracle 的 log_time 變成 `audience_expansion_forbidden`。
- D5 **`PATCH /admin/ai/providers/{id}`、`PUT /admin/ai/settings`** 先查站台管理員再驗 body 與 id（`ai/admin.rs`）。
- D6 **HTTP 涵蓋**：以 OpenAPI 全部寫入路由為母體。表外的路由要在「只動自己的東西」或「前面階段已測」清單，各附理由；拿掉永遠比對不到的項目。`/delegations*`、`/agent-contexts*`、`/executions/{id}/undo`、`/changesets/*`、`/commands`（body 裡放真實與不存在的 id）都加進比對；`/admin/ai/providers/{id}` 三條用真實 provider id（在 `m1_11_gate_ai`）。`revise-and-apply` 先驗 `revision_review_token` 簽章，真實與不存在的 id 都是 400，回應相同。
- D7 **canary**：c2 拿掉 `spent_hours`，改讀 `cf:Internal`（`issue_view` 的 `fields` 用欄位名稱，不是 id）；RFIELD 改用 `update_fields` 寫入；兩個 canary 測試都加正向對照：有權限的人讀得到每一個 canary。
- D8 **T-M1-120**：OTHER 用自己的假伺服器；攻擊者伺服器收到的請求數 = 0（不只是不帶 key）。
- mutation 確認（各自暫時還原，跑 `m1_11_gate`，再復原）：
  - D1 還原 → `t_m1_66_existence_oracles_beyond_the_matrix` 失敗。
  - D2 還原（destination 不帶範圍）→ 同上失敗。
  - D3 還原（parse 不檢查看不到的目標）→ 同上與 `t_m1_66_http_m1_routes_for_a_person_without_rights` 失敗。
  - D4 拿掉 `row.user_id == me &&` → `t_m1_66_command_identity_matrix` 失敗。
- 已知的小差異：工時的 `not_found` 訊息裡是單的 uuid，不是呼叫者寫的 key（原本就這樣）。比對時去掉 id 後完全相同。

### 測試

- `m1_11_gate.rs`（8 個，不需要 feature）：
  - `t_m1_66_command_identity_matrix`（13 身分 × 6 命令）
  - `t_m1_66_existence_oracles_beyond_the_matrix`（獨立驗證 V1：46 組「看不到 vs 不存在」比對，涵蓋缺 precondition、`project_id` 不一致、agent 提案的 destination）
  - `t_m1_66_more_command_shapes`（triage decline／duplicate／snooze／unsnooze、只帶 `project_id` 的 log_time、同時帶單與 `project_id`、`update_time_entry` 改單）
  - `t_m1_66_propose_new_ops_off_and_paused`（V3：MCP `changeset_propose`／`revise` 帶新命令）
  - `t_m1_66_removed_member_and_settings`（V5）
  - `t_m1_66_reused_key_is_no_existence_oracle`（用過的 key＋新文件指向看不到的單，有沒有 precondition，都和不存在相同；人與 agent）
  - `t_m1_66_http_every_write_route_refuses_agents_and_strangers`
  - `t_m1_66_http_m1_routes_for_a_person_without_rights`
  - `t_m1_67_agents_cannot_name_people_or_decisions`
  - `t_m1_68_idempotency_of_new_commands`
  - `t_m1_69_prompt_injection_external_agent`
  - `canary_notifications`
  - `t_m1_127_reserved_handles`
- `m1_11_gate_ai.rs`（7 個，feature `ai`）：
  - `t_m1_66_http_admin_ai_real_ids_for_a_non_admin`（VA1：真實與不存在的 provider id 回應相同且 403；settings、建立 provider 一律 403；沒有寫入）
  - `t_m1_66_builtin_ai_column`
  - `t_m1_122_no_direct_write_after_reading_time`
  - `t_m1_69_prompt_injection_builtin_ai`
  - `canary_ai_everywhere`
  - `t_m1_120_key_only_to_its_registered_host`
  - `t_m1_119_names_and_addresses`
- `m1_11_e5.rs`（feature `ai`）：
  - `e5_harness_self_check`：假 provider；參考答案全過、空答案全不過，兩種跑法都驗。
  - `e5_live`：ignored，要 `OXLEDGER_E5=1`。
- 共用的假 provider 移到 `tests/ai_fake/mod.rs`。`m1_10_ai_run.rs` 沒改，仍用自己那份。
- 引用、沒有重寫的既有測試：
  - T-M1-121：`m1_9` 的 `t_m1_121_one_runtime_per_client`（`tokio::join` 並行）與 `v_q2_trigger_concurrent_sql`（兩條 SQL 連線並行）。
  - T-M1-127 主體：`t_m1_127_builtin`。
  - T-M1-104：`t_m1_102_audience_and_injection`。
  - 通知撤權：T-M1-74。
  - 別人的對話與 run：`iv8_not_found_is_identical`、`t_m1_108`。
- 沒測到：
  - 核心情境 ≥ 90%：沒有真模型，還不能驗證。
  - 通知沒有 SSE 端點，所以沒有測 SSE 的 canary。
  - `OXLEDGER-AI@` 大寫 handle 只透過 API 測了（API 做不出來），沒有直接寫 DB 去測 trigger。
  - HTTP 矩陣只用一個沒有權限的使用者；不同角色組合的細部差異沿用各階段自己的測試。
