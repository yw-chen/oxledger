# S8 發布門檻：檢查清單與疑點

契約 02 §10 S8：E4w-1…8、T-S1、T-S2、T-D1…T-D14、T-V1…T-V35 全綠；越權、繞過政策的寫入、重複效果 = 0。

## 範圍（S8 前的對照，子代理盤點）

| 狀態 | 項目 |
|---|---|
| 未實作 | T-S1（搜尋 `consistent_after`／`search_lagging`）、T-V17（`overdue`）、T-V18／18b（`version:` 與 `project_scope_required`）、T-V5（`fixed_version.deleted`）、T-V16（歷史「（已刪除）」）、T-V13 委派頁可加欄位的提示 |
| 已實作沒測 | E4w-3、T-S2、T-V14、T-V23、T-V27、T-V30 跨專案半邊 |
| 部分 | E4w-2/4/5/6、T-V1/2/3/4（另一種先後）/5/6/8/15/22/31 |
| 缺 | E4w-7（tracing 半邊）、T-V21（壓力） |
| 契約沒有 | T-V20 |

## 疑點與照建議的做法（「全部照建議」）

- D1 搜尋一致性只做 03 §4.2 的記憶體 frontier：每輪同一個 statement 取 H 與事件；F 只在 reader reload 之後發布（`tokio::sync::watch`）。
  - durable payload、藍綠重建、outbox 清理、指標不在 S8（03 §4.8–4.10 是 E6／後續）。
  - 重啟後 F 從 0 開始，第一次 sync 後推到 H：只會讓等待者多等，不會錯（03 §4.2 允許保守）。
- D2 等待上限：每個呼叫者 4、全域 1000（03 §4.3）。超過就立刻回 lagging。
- D3 `wait_ms` 0–2000，預設 1000；`on_lag` 預設 `return`（03 §4.4 待確認值照建議）。
- D4 等待在讀取交易之外做，避免拿著連線等。
- D5 Filter AST 的內建欄位 `fixed_version_id`、`start_date`、`due_date` 用和自訂欄位同形狀的 EXISTS（對 `issues` 本身），所以 ne／is_null 的語意和自訂欄位一致。
- D6 `overdue` 與 `version` 是搜尋的頂層參數；文字語法（B2 最小語法）加 `version:` 鍵（已存的分頁文字若有 `version:` 字樣，以前算自由文字，現在算條件）。`overdue` 用 `true`。`meta.today` 放在回應。
- D7 `version:` 限縮條件：參數 `project` 或文字語法 `project:` 恰好一個。Filter AST 裡的 `project_id eq` 不在第一切片的 Filter AST（AST 沒有 project 欄位），所以只認參數。
- D7b `version:next` 沒有可用版本（只有無日期或沒有 open）→ `invalid_filter`（`reason: no_next_version`）；契約沒寫，照建議不回空結果，免得 agent 以為「下個版本沒有單」。
- D8 歷史裡已刪除的版本：`{id, name: "v2.0（已刪除）", deleted: true}`。
- D9 T-V13「人在委派頁加入欄位後成功」：後端是更新委派的 writable_fields；沒有 SPA，測 API。
- D10 T-V21 壓力測試用固定種子的隨機序列、4 個並行工作者、每個 200 步，檢查 R1、R4、R5 與沒有死結（逾時）。
- D11 E4w-7 tracing：用測試用 subscriber 收集所有 span／event 欄位，確認沒有隱藏內容。

## 獨立驗證之後（2026-11-09）
- 驗證者做了 45 個 mutant，S8 測試原本抓到 28 個。這次補上 `tests/s8_gate_probes.rs`（9 個）和兩處斷言（M38、M41/M42），抓不到的只剩等價 mutant M31 和已宣告的 M35。M05（publish 不單調）的情境已經消失：sync_round 現在一次只跑一輪。
- 行為修正：落後時輸入錯誤優先回傳；版本名稱保留 `next`；sync_round 一次只跑一輪；新增錯誤注入點 `search.before_publish`。
- fast field 延後到 S8 之後（見 decisions.tsv）。
