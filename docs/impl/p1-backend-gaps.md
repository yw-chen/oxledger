# P1 前端：後端缺口與 bug

做前端第一切片時發現的後端缺口。「已修補」是在 `claude/p1-frontend-a9aa9b` 分支上做的最小修補（附測試）；「未修」的前端先用替代做法，需要後端決定或較大改動，交給 ox-backend session。

| 編號 | 狀態 | 缺口 | 前端怎麼處理 | 建議 |
|---|---|---|---|---|
| G1 | 已修補 | 沒有給議題頁用的讀取點：`GET /issues/{key}` 不帶 fields 時不回 priority、fixed_version，也沒有 status_id／tracker_id／assigned_to_id／priority_id，表單無法設定初值 | 新增 `GET /issues/{key}/view`（`SpaIssue` = IssueView 全欄位 + project_key／ids），同一個交易裡先做可見性檢查；`include`、`note_limit`、`event_limit` 參數錯誤回 invalid_argument | 之後若要合併回 `GET /issues/{key}`，保留現在的欄位名稱 |
| G2 | 已修補 | 沒有「我看得到的專案」清單 | 新增 `GET /projects`（`{id,key,name}`，依 key 排序，只含 scope 內的專案） | — |
| G3 | 已修補 | `GET /agent-contexts` 的 ContextView 沒有 agent 名稱與暫停狀態，委派頁與左側欄的「!」標記畫不出來 | ContextView 加 `agent_label`、`suspended_at`、`suspension_reason`（LEFT JOIN agent_context_write_controls） | — |
| G4 | 未修 | 沒有個人偏好端點（首頁小工具、主題、左側欄收合） | 存 localStorage `ox.prefs.<userId>`，換瀏覽器不會跟著走 | 加 `GET/PUT /me/preferences`（整份取代、帶 version） |
| G5 | 未修 | 沒有 D6 所說的 layout JSON | 依 tracker schema 的 `type` 直接渲染欄位，順序照 schema | 若要自訂版面，後端送 layout |
| G6 | 未修 | `POST /issues/search` 只收結構化參數，不收文字語法（03 §11）；儲存的篩選分頁才由伺服器正規化 | 前端移植 `query_syntax.rs`（相同語法與錯誤位置，單元測試對照 Rust 測試） | 加 `q_syntax` 參數，讓伺服器回語法錯誤位置，前端就不用維護兩份解析器 |
| G7 | 未修 | `EventView.actor` 只有顯示名稱，沒有 kind（人／agent）與 on_behalf_of | 歷史事件只顯示名稱，不標「agent」（不用字串規則猜，避免把名字含 @ 的人誤標成 agent） | EventView 加 `actor_kind`、`on_behalf_of` |
| G8 | 未修 | 沒有 tracker 清單端點 | 用 `POST /resolve {text:"",kind:"tracker"}`（子字串比對會列出全部，前端依 id 去重） | resolve 有 `LIMIT 20`，別名多時會漏 tracker；建議加 `GET /projects/{id}/trackers`（只列專案啟用的） |
| G9 | 未修 | 議題頁 note 最多 10 則、事件最多 20 筆，沒有分頁 | 顯示「只顯示最近 N 則」 | 加 cursor |
| G10 | 未修 | seed-demo 的角色只有 view_issues／manage_project，無法走寫入流程 | E2E 用 `web/e2e/fixture.sql` 補權限（不改 seed） | seed 補上第一切片需要的權限，或提供 `seed-demo --with-write-roles` |
| G11 | 已修補 | 待審佇列的 diff：自訂欄位的「提案值」沒有轉成顯示值（存下來的提案是 `{"list": "<id>"}`，`render` 只認純值），list 欄位會顯示 option id、數字顯示成 `{"float":"5"}` | `review_queue::render` 先拆掉型別外殼，int／float 轉成數字（和「現在」欄一致）；`tests/s6b_queue.rs` 加 list／float／user／string 的斷言 | — |
| G12 | 未修 | `GET /changesets/{id}` 沒有 diff 與顯示名稱 | 提案頁自己讀每張目標單的 view、schema、成員、里程碑，把 id 換成名稱 | 詳細頁也回 `diff`（和列表同一個函式） |
| G13 | 未修 | 提案沒有記錄「原本」的值（只有 agent 自己在 precondition 帶 `field_values` 才有） | 衝突三欄的「原本」常顯示「提案時沒有記錄」；「你提案的欄位都沒變」只在有原本值時判斷 | 提案時由伺服器記下 readable 欄位的基準值 |
| G14 | 未修 | 沒有列出 agent token 的端點 | 委派頁只能撤銷「這一頁發出的」token；之前發的 token 只能靠停用 agent 或建立新工作脈絡失效 | 加 `GET /agent-contexts/{id}/credentials`（id、label、建立與到期時間、最後使用時間，不含 token） |
| G15 | 未修 | 沒有「加入這個專案後 agent 能寫到哪裡」的預覽端點（§15.2 方向預覽），也沒有建立前的資格檢查 | 精靈只顯示一般說明；專案不符合條件時由建立委派的錯誤回報 | 加 `GET /delegations/preview?agent_client_id=&project_id=`（可讀／可發布兩欄、資格） |
| G16 | 未修 | MCP `get` 的 fields 不接受 `start_date`、`due_date`（agent 可以寫但讀不到） | E2E 腳本改讀 lock_version | `get` 補上這兩個欄位 |
| G17 | 未修 | 活動頁 diff 的自訂欄位是原始型別值、欄位是 `cf:N` | 前端合併各專案 schema 換成名稱 | 和佇列 diff 用同一個 render |
| G18 | 未修（測試） | `tests/s6_read.rs` 的 10 個測試並行時互相清掉 DB（`issue_statuses_name_key` 重複），預設 `cargo test` 會失敗並中斷後面的測試檔 | 全套用 `--test-threads=1 --no-fail-fast` 跑（153 個全過） | 這個檔加上其他檔用的序列鎖，或合併成一個測試 |
| G19 | 已修補 | tracker schema 沒有說哪些欄位可以放進 agent 委派；精靈把 `visible = false` 的自訂欄位放進 `writable_fields`，建立委派被拒（E2E 發現：AUTH 的 Customer 欄位） | `FieldSchema` 加 `delegable`（內建欄位 true、`status_id` false、自訂欄位 = `visible`）；`tests/s3_permissions.rs` 加斷言；`api/` 重新產生 | — |
| G20 | 未修（待查） | E2E 期間後端 log 出現 `there is already a transaction in progress` 接著 `SET TRANSACTION ISOLATION LEVEL must be called before any query`（`service::read_tx`），請求回 500；發生在提案頁連續開啟、核准、修訂（流程 4）時。用 MCP 讀取＋中途取消 240 次沒有重現 | 前端的重試與 ErrorBox 讓流程照樣完成；E2E 仍全過 | 查哪條路徑讓連線帶著未結束的交易回到連線池（候選：請求被瀏覽器換頁取消時正在 `begin()`；某處 `acquire()` 後手動開交易） |
| G21 | 未修（環境） | Windows 上 E2E 第一次跑時 tantivy 寫索引 `PermissionDenied (os error 5)`，`index sync failed` 警告；之後的同步成功 | 不影響（搜尋走 DB 複驗） | 可能是防毒軟體鎖檔；重試或把索引目錄排除掃描。同類現象：後端全套單執行緒跑時 `s7_redaction::search_never_shows_redacted_text`、`s8_gate::t_s2_outbox_consumed_twice` 各失敗一次（索引 20 秒內沒追上），單獨重跑兩次都通過 |
| G22 | 未修 | 時程 `range.truncated_by_version` 回字串 id，契約 §17.12 寫的是 `{id,name,effective_date}` | 前端另讀 `/projects/{id}/versions` 把 id 換成「名稱（日期）」 | 照契約回物件 |
| G23 | 已修補 | 里程碑清單與健康度門檻沒有「呼叫者能不能改」的旗標，沒有 manage_versions 的人也看到「新增里程碑」「改日期」「儲存門檻」，送出才被擋（F6 驗證發現） | `VersionList.can_manage`、`ScheduleSettings.can_manage`（= manage_versions）；`tests/s6b_schedule.rs` 加 viewer／manager 斷言；`api/` 重新產生 | — |
