# P1 前端第一切片：階段計畫

2026-11-09。範圍依設計 02（§4、§14、§15、§16、§17、§18、§19）與設計 01 §15.1、§15.11；原型（設計 04）只當流程參考，有出入時以 02、03 為準。內建 AI 依前端 Q2「關閉」：右欄只做 agent 動態，AI 對話分頁顯示關閉狀態。

## 技術選擇

| 項目 | 選擇 | 理由 |
|---|---|---|
| 框架 | SvelteKit 2 + Svelte 5，`adapter-static`、`ssr = false`、SPA fallback | D6 |
| 位置 | `web/`（獨立 npm 專案） | 不碰 Rust workspace |
| API 型別 | 直接 import `api/types.ts` 的 `Endpoints`，client 依「方法 + 路徑」推導 body 與回應型別 | 型別由 Rust 產生（D6），前端不手寫 DTO |
| 開發 | Vite dev server `127.0.0.1:5174`，`/api` 代理到後端；後端 `OXLEDGER_SPA_ORIGIN=http://localhost:5174` | 同源才能用 `__Host-` cookie 與 CSRF |
| 元件測試 | Vitest + @testing-library/svelte + jsdom | |
| E2E | Playwright（Microsoft），對真後端與真 PostgreSQL（DB `oxledger_fe_e2e`） | |
| UI 套件 | 不用；樣式沿用原型的設計 token（深色預設、淺色可切） | 依賴少，D17 好審 |

## 不放進 P1（以 02／01 §15.1 為界）

內建 AI 與右欄 AI 對話（Q2 關閉，只顯示關閉狀態）、Triage 收件匣（U26 未定）、品牌、表單設計、AI 團隊、看板／Gantt／mindmap／BPMN、自訂欄位管理工具（Q-S9 的管理工具不在 02 的 REST 範圍）、通知與寄信、窄螢幕抽屜（U23 只做基本收合）、D12 遮蔽的管理 UI（端點已有，02 §14 的主要人類流程不含，列為後續）。

## 階段

| 階段 | 內容 | 對照 |
|---|---|---|
| F0 | `web/` 骨架、typed API client（envelope、錯誤物件、CSRF、Idempotency-Key／operation_id）、登入（login-context → login）、session 到期回登入再回原頁、登出、主題切換 | 02 §3.3、§4.3、§15.1 |
| F1 | 版面 B（左導覽可收合、中工作區、右協作欄「AI 對話｜動態」）；首頁小工具（可新增、移除、排序、寬窄；「需要你決定」固定不能移除）；⌘K 意圖列（key 直接開、結構化語法查詢、自然語言改走文字搜尋並標示） | 版面 B、U22、01 §6、§15.11 |
| F2 | 議題搜尋（命中欄位、高亮、部分字元相符、沒有總數、`truncated`、`search_lagging`）；議題頁（屬性、自訂欄位、note、歷史、專案框架與「← 回到」） | 03、02 §18.6 |
| F3 | 人直接操作：改欄位、轉狀態（只列合法的）、note（可私有）、標重複、回報問題（建單）；`conflict` 不覆蓋、保留輸入；§15.5 錯誤訊息 | 02 §6.8、§15.5 |
| F4 | 待審佇列（徽章、`review_modes`、勾選規則、最多 4 個並行、逐列結果與摘要、停止）；提案頁（diff、檢查、核准並套用、拒絕、逾時以原 operation_id 查結果）；衝突頁（三欄、逐欄選、`revision-previews`、建立修訂版並核准套用、只儲存修訂版、`review_outdated`） | 02 §8.6、§15.3、§15.4 |
| F5 | 委派精靈（選 agent → 選專案 → 操作表 → 確認；整個精靈只 step-up 一次；token 與 MCP 設定只顯示一次）；委派頁（三種停止、暫停／恢復、新工作脈絡宣告、可加入的新操作）；Agent 活動頁（diff、undo 預覽 → 回復、事故回復）；首頁 agent 摘要與暫停卡 | 02 §15.2、§16.7、§16.10 |
| F6 | 專案框架與分頁（時程、里程碑、議題、開發；儲存篩選與連結分頁的管理、並行衝突）；時程視圖；里程碑建立／改日期（impact）／關閉；健康度門檻；GitHub 開發分頁、議題頁開發卡、連動 repo、系統管理員開放 repo | 02 §17、§18、§19 |
| F7 | 錯誤畫面一覽對照、帳號頁（session 清單、登出其他裝置）；E2E 串起 02 §14 主要人類流程；preview 截圖 | 02 §15.5、§15.1 |

每階段：實作 → 元件測試 + 對真後端的 E2E → 沒寫過這段的子代理獨立驗證（實際跑、做 mutation）→ 修確認的缺陷 → commit。

## E2E 的資料

`web/e2e/setup` 在全新 DB `oxledger_fe_e2e` 上：套 migration（`serve` 啟動時自動）→ `seed-demo` → `user set-password` 給 alice／bob／admin → 起 `serve`（port 18080）→ Vite preview（port 5174）。agent 端用 seed 印出的 key 或 SPA 委派精靈發的 token，透過 `/mcp` 呼叫 `command_submit`／`changeset_propose` 產生 agent 側資料。

## 後端缺口

見 `p1-backend-gaps.md`。
