# S6b 實作清單：審閱便利、undo、時程、專案分頁、GitHub 連動（契約 02 §10 S6b）

> 來源：`docs/design/02-first-slice-contract.md`（3760 行；以下「L123」都指這份檔案的行號）。對照的程式碼是分支 `s6b-review`（HEAD `0527774` S6）。
> 這份清單只整理契約與現況，不是新設計；有疑義的地方列在 §4（B1…），沒有自行決定。
> S6b 的範圍定義：L1805。**只做後端**（REST、MCP、DB、worker），不做 SPA 畫面；SPA 要用的 API 與資料要齊。
> 完成標準：L1805 寫的是 T-U1…T-U12、T-M9…T-M20；本清單另把 §18 的 T-PT1…T-PT17、§19 的 T-G1…T-G36 列為完成標準（任務要求）。T-D6…T-D14、T-V9…T-V12、T-V28…T-V35 是 S8 的門檻（L1807），但測的是 S6b 做的東西，建議在 S6b 先寫。

---

## 0. 一頁摘要

**大小估計**：比 S6 大。粗估正式程式約 9,000–11,000 行、測試約 7,000 行、migration 4 個（0013–0016）。其中 GitHub 連動（§19）大約占一半：它是第一個要對外連網的功能，也是第一個需要新 crate 的功能（§3.10）。

**已經有的骨架**（S3–S6 先做了）：
- `changeset_decisions.review_mode` 的 CHECK 已含 `detail`、`queue`、`revise_and_apply`（0009 L130）。`ApproveInput.review_mode` 已能解析，但 `accept()` 只收 `detail`（changesets.rs L3397）。fingerprint 已把 `review_mode` 算進去（L3211–L3214）。
- 交易 A／B／C、冪等、故障注入點（`faults::Gate` 屏障；`approve.after_a` 等點）都在。T-U9、T-U11 可以直接用。
- `ErrorCode::ReviewOutdated`、`VersionSuperseded`、`ApprovalInvalidated` 已有。
- undo 需要的表全部在 0008：`execution_refs`、`command_executions.reverts_execution_id`、`command_execution_source_projects`、`issue_field_origins`（任何寫入都會更新，commands.rs L3107）、`execution_field_changes`（**只有 agent 寫入**才記，commands.rs L3121–L3150）、`issue_duplicates.id／created_by_execution_id`、`execution_relation_changes`、`version_field_origins`、`execution_version_changes`（versions.rs L2105）。
- 暫停與解除：`POST /agent-contexts/{id}/suspend|resume`（agents.rs L1060、L1086），`agent_context_write_controls`、`agent_write_log`（0012）。
- 版本 stats、health、門檻的讀取（versions.rs L376–L497，`health_table` 單元測試）；`project_schedule_settings` 每專案一列與 trigger（0007 L87–L108）；`org_settings.schedule_lock_version`（0007）。
- agent 回應過濾器（changesets.rs `filter_for_agent` L2074、`agent_view` L1798）與 `content_unavailable: "owner_revised"`；s6_mcp.rs 已測一部分（L256–L330、L400–L470），但沒有 T-M 編號。
- `status_since`／`last_activity_at` 已做，**T-U8 已在 S6 通過**（tests/s6_read.rs L1207）。T-M14 也已通過（tests/s6_evidence.rs L103）。
- chacha20poly1305 加密 cursor，金鑰在 `server_secrets`（0006；cursor.rs L59）。預覽 token 可以用同一套機制。

**完全沒有的**：
- 佇列：`review_modes` 計算、列表上的欄位 diff、`group` 過濾與 cursor、步驟 13a、`evidence_changed` 警告。
- `revision-previews`、`revise-and-apply` 兩個端點。
- undo、事故回復、Agent 活動頁、首頁摘要卡的任何 service 與端點（§4.2 也沒有列，見 B3）。
- 時程視圖 `/projects/{id}/timeline`；`/projects/{id}/schedule-settings`、`/org/schedule-settings`（versions.rs L14 註明在 S6b）。
- 專案分頁的全部：`manage_project` 權限、`project_tab_settings`、`project_setting_events`、端點。
- GitHub 連動的全部：11 張表、webhook、worker、GitHub API client、REST、MCP `include: ["dev"]`。
- S6 延後的配額：validate 每委派 120／小時、同委派同時提案寫入 2、payload 768／512 KiB、每人與全組織總上限（decisions.tsv「S6.9：§8.9 propose 類只做…」那一列）。

**建議切成可以各自 commit 的子步驟**（每步 `cargo test` 含 PG18 整合測試全綠才進下一步）：

| 步 | 內容 | migration | 估計（程式＋測試） | 測試 |
|---|---|---|---|---|
| S6b.1 | 待審佇列：`review_modes`、列表 diff、`review_mode: "queue"`、步驟 13a | — | 1,000＋700 | T-U1、T-U2、T-U3（server 部分）、T-U4、T-U9 |
| S6b.2 | `revision-previews`＋`revise-and-apply` | — | 900＋700 | T-U5、T-U6、T-U7、T-U10、T-U11、T-U12 |
| S6b.3 | agent 出口回歸與 S6 剩下的配額 | 0013 | 300＋900 | T-M9…T-M13、T-M15…T-M20；§8.9 validate、同時 2、payload |
| S6b.4 | issue 的 undo、事故回復、Agent 活動頁、摘要卡 | 0014 | 1,600＋1,000 | T-D6、T-D7、T-D8、T-D12（T-D9、T-D13、T-D14 要等 S7） |
| S6b.5 | 時程視圖、健康度門檻端點、里程碑 undo | — | 900＋700 | T-V9、T-V9b、T-V10、T-V11、T-V12、T-V28、T-V29、T-V33、T-V34、T-V35 |
| S6b.6 | 專案分頁 | 0015 | 700＋600 | T-PT1…T-PT16（T-PT17 等 S6b.9） |
| S6b.7 | GitHub：資料模型、webhook 收件、worker、正規化與彙總 | 0016 | 1,600＋1,200 | T-G1…T-G8、T-G19、T-G20、T-G22、T-G25、T-G26、T-G32、T-G33、T-G35、T-G36 |
| S6b.8 | GitHub：API client（App JWT、installation token）、回填、對帳、補送、rate limit | — | 1,500＋1,200 | T-G13、T-G15、T-G21、T-G30、T-G31、T-G34 |
| S6b.9 | GitHub：連動 REST、開發分頁、隱藏與撤除、admin、MCP `get(include:["dev"])` | — | 1,000＋900 | T-G9…T-G12、T-G14、T-G16、T-G17、T-G18、T-G23、T-G24（部分）、T-G27、T-G28（見 B15）、T-G29、T-PT17 |

S6b.1–S6b.6 不需要新 crate；S6b.7 需要 `hmac`（已在 Cargo.lock）；S6b.8 才需要 HTTPS client 與 RS256（新下載）。建議把 S6b.7–S6b.9 開成獨立分支，先讓使用者決定 §3.10 的 crate。

---

## 1. 契約條文與測試期望

### 1.1 待審佇列與佇列核准（§4.2、§6.7 步驟 13a、§8.6、§15.3）

| 條文 | 行 | 要點 |
|---|---|---|
| `GET /changesets?bucket=awaiting_me&group=&cursor=` | L452 | 每列附 `review_modes` 與欄位 diff；可依分組過濾；有 cursor |
| `POST /changesets/{id}/approve-and-apply` | L457 | body 可帶 `review_mode: "queue"` |
| `review_outdated` | L530 | 409；「佇列核准時該份已不符合 `queue` 資格」；不可重試（重新預覽） |
| 步驟 13a | L965 | `review_mode = queue` 時，交易 B 寫入前再判一次資格：操作白名單、沒有警告、evidence 的 citation **以執行者範圍** `exact` 驗證仍是 `valid`；失格 → execution failed、`error_code = review_outdated`、零效果 |
| DDL | L1252 | `changeset_decisions.review_mode text NOT NULL DEFAULT 'detail' CHECK (review_mode IN ('detail','queue','revise_and_apply'))`（0009 已建） |
| 兩個時間窗 | L1515 | 接受**後**才發現的失格 → execution failed、HTTP 200；接受**前**（交易 A）判定的 `review_outdated` → HTTP 409、**不建任何列** |
| 多份核准 | L2081 | SPA 對每份各送一個 approve-and-apply（各自 operation_id、version、hash、`review_mode:"queue"`），最多 4 個並行；**沒有批次端點** |
| 誰能被勾選 | L2082 | server 在列表回 `review_modes`，含 `queue` 才能勾：validation 通過、沒有警告（evidence 以 owner 身分重驗不是 `valid` → 警告 `evidence_changed`，不擋單筆套用）、diff 在列表完整顯示、只有短欄位更新（不含 subject／description）與單次 transition。長文、note、建立、duplicate、私有設定、blocked、conflict 都不行。server 在交易 A、交易 B（13a）各驗一次 |
| 勾選規則 | L2083 | 預設不全選；同一次選取不能有兩份改同一張 issue；改版後自動取消勾選；勾選綁 version＋hash（前端） |
| 中途出錯 | L2085 | 401、CSRF、服務異常 → 停止送出剩下的；已送出的繼續查原結果（前端） |
| 每列顯示 | L2079 | 目標 key＋標題、欄位**顯示名稱**與前後值（同名的人附帳號）、根據、警告、檢查狀態與時間 |

**測試**

| # | 行 | 情境 → 必須成立 |
|---|---|---|
| T-U1 | L1767 | 偽造 `review_mode:"queue"` 核准一份含 note 或 description 改寫的提案 → `review_outdated`，零效果（交易 A 判定，HTTP 409，不建 decision、execution、operation_request） |
| T-U2 | L1768 | 佇列勾選兩份改同一張 issue 的提案 → 前端擋；server 端第二份照一般規則 `conflict`，不產生意外效果 |
| T-U3 | L1769 | 佇列核准 8 份，第 4 份送出時 session 過期 → 已送出的照原結果；未送出的標「未送出」；不重試成新 key（後端只能測：過期 session 送出 → `unauthenticated` 且零列；原 key 查詢仍回原結果） |
| T-U4 | L1770 | 勾選後提案被 agent 修訂 → 若仍送出 → `version_superseded` |
| T-U9 | L1775 | 佇列核准：交易 A 提交後、B 驗證前（barrier），修改 evidence 引用的**另一張** issue 的描述 → execution failed、`review_outdated`、零效果；HTTP 200 回 failed 的 execution |

### 1.2 衝突頁：`revision-previews` 與 `revise-and-apply`（§4.2、§8.3、§8.5、§15.4）

| 條文 | 行 | 要點 |
|---|---|---|
| `POST /changesets/{id}/revision-previews` | L458 | 依人的逐欄選擇算出完整修訂內容，回短效 `revision_review_token`；**不寫入** |
| `POST /changesets/{id}/revise-and-apply` | L459 | 建立修訂版並核准套用，一次操作 |
| 只有原始核准請求能恢復 | L1371 | 用同一個 operation_id 重送 `approve-and-apply` **或** `revise-and-apply` 才會繼續 queued execution；GET 不寫 |
| 衝突三欄 | L1440–L1446 | `details.review` 給 `{original, current, proposed}`，只含可讀欄位；`fields_unchanged` |
| owner 修訂 | L1450 | `edit_method = owner_revise`；origin、委派、脈絡、ceiling、期限不變；新版來源＝目前 R（⊇ 原版）；脈絡停用 → `context_unavailable` |
| 預覽綁定 | L2095 | server 回**完整**最終修訂內容（含沒衝突的 op、note、evidence）＋token，token 綁 owner、ChangeSet、來源版本／hash、完整修訂內容摘要、目前 `lock_version`、ceiling、epoch、期限 |
| 交易 A 順序 | L2096 | action ＝ `revise_and_apply`；fingerprint 含來源 version、`expected_current_version`、正規化後的完整修訂內容與 token 摘要。① 認證、讀取授權；② **先查 `operation_requests`**：同 key 同 fingerprint → 取既有新版本與 execution，繼續 B 或回終態，**不再驗 token、不再建版本**；同 key 不同 action 或內容 → `idempotency_key_reused`；③ 新請求才鎖：ChangeSet → 脈絡 `FOR SHARE`（active）→ principals／session／delegation → projects（目前 R ∪ 目的地，UUID 序 `FOR SHARE`，比全部 epoch；新版來源＝目前 R）→ 目標 issues `FOR SHARE`，讀**目前** `lock_version` 與前置值，用 token 綁定的條件重算預覽承諾；④ 相符才 INSERT 新版本（`owner_revise`）＋decision（`review_mode = revise_and_apply`）＋queued execution，同交易提交；之後走 §8.6 的 B／C |
| 預覽之後又變了 | L2097 | ③ 不符 → `review_outdated`（409），不核准、不建版本；A 提交後、B 開始前的變更 → B 照一般規則 `conflict`（execution failed，新版本留著） |
| 只儲存 | L2098 | 另一顆「只儲存修訂版」＝既有 `POST /revisions` |
| 限制 | L2101 | owner 修訂仍受原 ceiling、委派、epoch、期限 |

**測試**

| # | 行 | 情境 → 必須成立 |
|---|---|---|
| T-U5 | L1771 | 預覽後另一人改了 issue，再按「建立修訂版並核准套用」→ `review_outdated`；不建新版本、不核准 |
| T-U6 | L1772 | 預覽 token 用在另一份 ChangeSet 或另一個 owner → 拒絕 |
| T-U7 | L1773 | revise-and-apply 成功 → 新版本、核准、execution 在同一個交易 A；`review_mode = revise_and_apply`；ceiling、期限不變 |
| T-U10 | L1776 | 預覽後目標 issue 被改，再送出 → `review_outdated`；`changeset_versions`、`changeset_decisions`、`changeset_executions` 筆數都沒增加 |
| T-U11 | L1777 | 交易 A 提交後殺掉程序，等預覽 token 過期，再用原 operation_id、原 body 重送 → 冪等命中：不再驗 token、不再建版本，繼續既有 queued execution 或回終態 |
| T-U12 | L1778 | 同一個 operation_id 先送 revise-and-apply、再送 approve-and-apply → `idempotency_key_reused`（action 不同） |
| T-U8 | L1774 | （S6 已完成）`status_since`：從未變過、事件被遮蔽、no_effect transition → 建立時間、`null`（`status_since_known:false`）、不更新 |

### 1.3 agent 出口回歸（§8.8 回應過濾、§9.10）

條文：§8.8 工具表 L1537–L1551；回應過濾 L1553–L1561（目標讀不到只回 `{op_id, status}`；本文只有 agent 自己的版本且目標都讀得到才回；`owner_revise` 版本只回 `version`、`state`、`edit_method`、`content_unavailable`，**不回 hash、碼與狀態**；三欄只回 `current`；套用後只看得到 `succeeded`／`failed`）。

| # | 行 | 情境 → 必須成立 |
|---|---|---|
| T-M9 | L1779 | `get_many` 帶 `include:["recent_events"]`、`event_limit: 3`，10 張各有 20 筆事件 → 每張最多 3 筆、標 `truncated` |
| T-M10 | L1780 | 委派只允許 `add_note`；agent `get(include:["actions"])`，委派者本人有 `edit_issues` → 只列 notes；欄位與轉移不出現（不是 false）；沒有任何「能套用」欄位 |
| T-M11 | L1781 | 提案套用後撤銷委派，agent 呼叫 `changeset_get`／`operation_get` → `delegation_unavailable` |
| T-M12 | L1782 | agent 提議建立**私有** issue，人核准成功，委派仍有效；`changeset_get`、`changeset_list`、`changeset_list(group_id)`、`operation_get`、propose 冪等重播 → 都只回 `{op_id, status}`，沒有新 issue 的 key、標題、欄位值 |
| T-M15 | L1783 | agent 提議私有 note；owner 修訂 note 本文加新文字 → agent 的 `changeset_get`（目前版本與指定歷史版本）、`changeset_list`、`operation_get` 都拿不到新文字，只看到 `content_unavailable: "owner_revised"` |
| T-M16 | L1784 | 公開 issue 的 description 提案被 owner 改成 Z；別人改 priority 造成衝突；agent 依序 validate、get、operation_get → 三欄只有 `current`；任何欄位、錯誤 details、摘要都沒有 Z |
| T-M17 | L1785 | owner 修訂版 `preconditions.field_values.description` 放 Z 且不符（存成 conflict）→ 所有回應與錯誤 details 都沒有 Z |
| T-M18 | L1786 | owner 只把私有 note 改成兩個候選之一（低熵），agent 用相同 op_id 反覆提交候選 → 任何出口都沒有 owner 版本的 `proposal_hash` 或衍生值 |
| T-M19 | L1787 | 脈絡含 WEB、AUTH 兩份有效委派；`changeset_list` 不帶參數 → 兩專案；撤銷 WEB 後只列 AUTH，請求不失敗；`group_id` 指向只含 WEB 提案的分組 → 空列表 |
| T-M20 | L1788 | 雙世界：只差 owner 草稿的隱藏前置值（A 或 B）；核准前 agent 呼叫 validate、get、list、operation_get → 兩世界回應逐位元相同（除 request id）；核准套用後只差 `succeeded`／`failed` |
| T-M13 | L1789 | 同一個錯誤（例如 `rate_limited`）分別從 REST 和 MCP 觸發 → MCP 錯誤物件和 REST 的 `error` 相同（含 `retry_after_ms`、`next_actions`、`message`）（見 B13） |
| T-M14 | L1790 | （S6 已完成） |

### 1.4 §8.9 配額（S6 延後的部分）

| 項目 | 行 | 值 |
|---|---|---|
| validate | L1572 | 每委派 120 次／小時 |
| 同委派同時進行的提案寫入 | L1574 | 2 |
| 原始 payload | L1575 | 768 KiB |
| 正規化後命令文件 | L1576 | 512 KiB |
| 每人、全組織總上限 | L1582 | 沒給值（S6 checklist A7 未解） |
| 冪等命中不重扣、開始前原子保留 | L1582 | — |

### 1.5 undo、事故回復、活動頁、暫停與摘要卡（§16.2、§16.5、§16.7、§16.10）

| 條文 | 行 | 要點 |
|---|---|---|
| 歸屬 | L2183 | 人執行 undo：actor＝人、`human_direct`，另記 `reverts_execution_id` |
| `execution_refs` | L2263–L2289 | 兩種 execution 共用身分表（0008 已建） |
| undo 對照表 | L2325–L2329 | `update_fields` 逐欄還原；`add_note` 撤回（一般讀者看到「已由 alice 撤回」，本文隱藏，事件保留；undo 資料受 D12 約束）；`duplicate_of` 移除**該次建立的那個關係實例**（no_effect 的不記錄、不能 undo）；`transition` 有合法轉移回原狀態才提供「轉回 X」，沒有唯一目標就讓人選；`create_issue` 不刪除，有合法轉移時提供「關閉為誤建」 |
| 欄位還原條件 | L2331 | 目前值＝該次 agent 寫入值 **且** 該欄位目前來源仍是那次 execution **且** 人目前有權修改 **且** 舊值現在仍合法 |
| DDL | L2333–L2362 | `issue_field_origins`、`execution_field_changes`、`issue_duplicates` 修訂、`execution_relation_changes`（0008 已建） |
| D12 衍生副本 | L2364 | `before_value／after_value` 是敏感位置；遮蔽後 undo 預覽回 `redacted`、活動 diff 顯示「內容已遮蔽」 |
| undo 的來源 | L2367 | undo execution 的來源快照＝被回復 execution 的來源快照（遞移），寫進 `command_execution_source_projects` |
| undo 的鎖 | L2368 | 被回復 execution 的來源快照專案 ∪ 目標 issue 的專案，UUID 序 `FOR SHARE` 並比對 epoch → 目標 issues `FOR UPDATE` → **之後才讀** `execution_field_changes`、`issue_field_origins`；不在持有 issue 鎖時反向鎖專案 |
| 流程 | L2371 | 預覽 → 人確認 → 交易內重查 → 只提交可還原的子集合（子集合本身要通過完整領域驗證；跨欄位相依不滿足 → 整筆 `undo_not_applicable`）；undo 是新的 human_direct execution |
| 事故回復 | L2372 | 「回復 claude-code@alice 自 HH:MM 起的變更」：預設先提示暫停；凍結 execution 清單；每張 issue 由新到舊；每筆各自交易、各自冪等、各自結果，可部分成功；「轉回」「關閉為誤建」不批次執行，列出來逐筆處理；SPA 功能，不是 agent 工具 |
| 錯誤碼 | L2373、L2402 | `undo_not_applicable`（只給 SPA）；`details.reasons` 逐欄：`changed_since`、`value_no_longer_valid`、`permission`、`redacted`、`expired` |
| 保留 | L2437（U18） | undo 資料 30 天；更舊的只能手動改 |
| 活動頁 | L2425 | 每個 agent 有「活動」分頁：直接寫入、送審、被擋、undo；每筆可展開 diff、可 undo；可篩時間與 issue |
| 首頁 | L2426 | 只放：agent 被自動暫停（需要處理）、「今天 claude-code@alice 改了 N 張」摘要卡（N 依**目前可見** issue 去重，可停用） |
| 停止 | L2428 | 「暫停寫入」（可恢復）、「撤銷 token」、「停用這個 agent」；暫停只擋寫入 |
| 回復 | L2429 | 單筆 undo 1 下＋預覽；事故回復：選起始時間 → 預覽 → 1 下執行，逐筆結果 |
| 版本的 undo 鎖 | L2759–L2761 | 人工 undo：session 與 principal `FOR SHARE` → 來源快照專案 ∪ 版本的專案 `FOR SHARE` 比 epoch → versions `FOR UPDATE` → 之後才讀 `execution_version_changes`、`version_field_origins`；**不要求**原 agent 的 credential、委派、脈絡有效，**不套** agent 配額與熔斷 |

**測試**（S8 門檻，建議 S6b 先寫）

| # | 行 | 情境 → 必須成立 |
|---|---|---|
| T-D6 | L2384 | agent 改高 → Bob 改中 → Carol 改回高 → 該欄位不回復 |
| T-D7 | L2385 | undo 一筆 direct、一筆送審套用 → 都能建 undo execution，`reverts_execution_id` 指得到 |
| T-D8 | L2386 | agent 建 duplicate → 人移除 → Bob 重建同兩端 → undo 原 execution → 新關係不被移除 |
| T-D9 | L2387 | WEB note 內容經 agent 寫進 AUTH.description，之後遮蔽該 note → 前後值與 undo 預覽讀不到原文（要 S7） |
| T-D12 | L2390 | 事故回復：清單固定後 agent 又寫入 → 新寫入不在清單內；逐筆可部分成功 |
| T-D13 | L2391 | E 把 AUTH.description 從 X 改成 Y；人 undo E；之後遮蔽 WEB 的來源 note → undo execution 的前後值、活動 diff、再 undo 的預覽都讀不到 X（要 S7；但 undo 的來源快照遞移在 S6b 就要對） |
| T-D14 | L2392 | undo 讀前後值與遮蔽交錯 → 遮蔽先：`redacted`；undo 先：遮蔽清掉新副本（要 S7） |

### 1.6 時程視圖、健康度門檻、里程碑 undo（§17.8、§17.12、§17.13）

| 條文 | 行 | 要點 |
|---|---|---|
| 端點 | L475–L476、L2678–L2681 | `GET /projects/{id}/timeline?from=&to=&versions=&include_closed=&cursor=`；`GET/PUT /projects/{id}/schedule-settings`（PUT 要 `manage_versions`，`{lock_version, inherit_org, health_at_risk_days?, health_at_risk_open_pct?}`、`Idempotency-Key`）；`GET/PUT /org/schedule-settings`（GET 任何登入者、PUT 系統管理員，帶 `lock_version`＝`org_settings.schedule_lock_version`） |
| 回應形狀 | L2832–L2843 | `{today, range{from,to[,truncated_by_version]}, lanes[{version{id,name,effective_date,status,health,health_reasons[,deleted]}|null, items[{key,subject,status{id,name,is_closed},assigned_to{id,name,kind},start_date,due_date,flags}], undated_count, next_cursor}]}` |
| 可見性 | L2847 | 只回 `load_scope` 看得到的；讀 DB 不經索引；私有看不到就不算進任何數字、不留空位 |
| 範圍 | L2848 | `[start,due]` 與 `[from,to]` 有交集；只有一端算那一天；兩端都沒有只算 `undated_count`；最長 400 天且 `from ≤ to`，違反 `invalid_argument` |
| 預設範圍 | L2849–L2856 | `from = today−14`；`d`＝日期 ≥ today 的 open 版本中最晚的；`to = d ? d+14 : today+90`；`to = min(to, from+399)`；被截斷附 `range.truncated_by_version`；只帶一端時另一端照算 |
| 泳道 | L2857–L2858 | 每版本一條（`effective_date` 排序，null 最後）＋「未排版本」；已刪除版本只在呼叫者看得到其中 issue 時出現、標 `deleted`；`include_closed=false` 不回已結案 issue 與已關閉版本；每條最多 200 筆，各自 `next_cursor`；排序 `(coalesce(start_date, due_date), key)` |
| flags | L2859–L2866 | `overdue`、`late_for_version`、`no_dates`（時程視圖不畫）、`in_closed_version` |
| health | L2868–L2878 | `closed` > `no_date` > `overdue` > `at_risk` > `on_track`；`health_reasons`：`version_overdue`、`issue_overdue`、`issue_late_for_version`、`deadline_near_incomplete`（versions.rs 已做） |
| 門檻 | L2880 | GET 回 `{effective, source: "org"|"project", org, project{inherit_org,…}, lock_version}`；PUT 照 `operation_requests` 冪等、CSRF；`lock_version` 不符 `conflict`；`inherit_org = true` 時另兩欄**必須省略**；`null` 停用；只有人能改 |
| 沒有 MCP | L2881 | agent 用 `version_list`（含 health、stats） |
| issue 的 undo 加條件 | L2889–L2891 | 還原 `fixed_version_id`＝新指定，舊版本要 open 且未刪除，否則 `value_no_longer_valid`＋`details.subreason = "version_not_open"`；還原日期要滿足 R4，只還原其中一個會違反 → 整筆 `undo_not_applicable`；undo 也走步驟 7b |
| 版本的 undo | L2759、L2765、L2965 | `create_version` 的 undo＝軟刪除（沒人改過且沒有看得到的引用）；`update_version` 逐欄（`version_field_origins`） |

**測試**

| # | 行 | 情境 → 必須成立 |
|---|---|---|
| T-V9 | L2943 | 兩張可見 issue 之間有一張看不到的私有 issue → 泳道、`undated_count`、stats、health 都和沒有那張時相同 |
| T-V9b | L2957 | 預設範圍：沒有版本；全部沒日期；open 版本都在過去；有 2199 年的 open 版本 → 都合法（`from ≤ to`、≤ 400 天）；最後一種附 `truncated_by_version` |
| T-V10 | L2944 | health 表格驅動（versions.rs 已有單元測試，補整合層） |
| T-V11 | L2945 | agent 把版本從 v1.9 改成 v2.0，之後 v1.9 被關閉，人 undo → `undo_not_applicable`，`value_no_longer_valid`／`version_not_open` |
| T-V12 | L2946 | agent 改 start 與 due，之後人只改 due；undo 只能還原 start 且會違反 date_order → 整筆 `undo_not_applicable` |
| T-V28 | L2964 | undo `update_version`：agent 改 → Bob 改 → Carol 改回 agent 的值 → 不回復 |
| T-V29 | L2965 | undo `create_version`：沒人改過且沒引用 → 軟刪除；有人改過名稱 → `changed_since`；有看得到的 issue → `in_use`；只有看不到的 → 軟刪除（見 B7） |
| T-V33 | L2969 | 門檻：組織 7／50；專案 3／80；停用；總數 0；剛好等於 → 依專案值；停用不出現；總數 0 不成立；等於 pct 不成立 |
| T-V34 | L2970 | 兩人同時改門檻：(a) 改過的專案；(b) 新建從沒改過的；(c) 組織 → 後送的都 `conflict` |
| T-V35 | L2971 | agent 改了 v2.0 日期；之後 agent 暫停、委派撤銷（另測到期、R 擴大）；人 undo → 照常成功：用原 execution 的來源快照與人目前的 `manage_versions` |
| T-V30 | L2966 | 遮蔽來源 note 後版本 undo 讀不到原文（要 S7） |

### 1.7 專案分頁（§18，U44）

DDL（L3022–L3046）：

```sql
CREATE TABLE project_tab_settings (
    project_id    uuid PRIMARY KEY REFERENCES projects(id),
    tabs          jsonb NOT NULL,
    default_tab   text  NOT NULL,
    is_default    boolean NOT NULL DEFAULT true,
    lock_version  integer NOT NULL DEFAULT 0,
    updated_by    uuid REFERENCES principals(id),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    CHECK (jsonb_typeof(tabs) = 'array' AND jsonb_array_length(tabs) BETWEEN 5 AND 15)
);
CREATE TABLE project_setting_events (
    id uuid PRIMARY KEY DEFAULT uuidv7(), project_id uuid NOT NULL REFERENCES projects(id),
    at timestamptz NOT NULL DEFAULT now(),
    kind text NOT NULL CHECK (kind IN ('tabs_updated','tabs_reset','repo_linked','repo_unlinked',
                                       'pr_hidden','pr_unhidden','github_content_revoked')),
    actor_id uuid NOT NULL REFERENCES principals(id),
    operation_request_id uuid NOT NULL REFERENCES operation_requests(id),
    payload jsonb NOT NULL,   -- tabs：{before, after}（完整設定）；repo：{link_id, repo_id, full_name}；pr：{pr_id, repo_id, number}
    redaction_id uuid
);
CREATE INDEX project_setting_events_idx ON project_setting_events (project_id, at DESC);
```

| 條文 | 行 | 要點 |
|---|---|---|
| tabs 每項 | L3051–L3058 | 內建 id 固定：`timeline`、`milestones`、`issues`、`dev`、`plan`；`query` 的 id＝`q_`＋10 碼 base32；`link` 的 id＝`l_`＋10 碼；欄位 `id`、`title`、`hidden`、`query`、`url` |
| R-T1 | L3063 | 五個內建每個剛好一次；`builtin_missing` |
| R-T2 | L3064 | 自訂最多 10 個；title 去頭尾 Unicode White_Space → NFC，1–20 字，不含控制字元；唯一性 casefold 後比（含內建名稱）；`tab_limit`／`tab_title_taken`／`tab_title_invalid` |
| R-T3 | L3065 | query 用 03 的搜尋語法解析、存正規化字串；必須有唯一的 `project:` 且等於本專案，沒寫由 server 補；執行時用開啟者的可見性；`tab_query_scope`／`tab_query_syntax`（見 B2） |
| R-T4 | L3066 | url 只收 https、無 userinfo、≤ 2048、WHATWG 正規化、不能指向 oxledger 自己的 origin；server 不抓取；`tab_url_invalid` |
| R-T5 | L3067 | `default_tab` 存在、未隱藏、非 link；`tab_default_invalid` |
| R-T6–R-T8 | L3068–L3070 | 不改任何資料；名稱、條件、網址算專案公開內容；`dev` 沒有連動時仍顯示 |
| 權限 | L3076–L3078 | 讀：專案可見（任一 `view_issues`），active 或 closed；寫：active＋**`manage_project`**（新權限，預設給 Manager，也管 §19 repo）；只用瀏覽器 session＋CSRF；agent 不能讀寫 |
| API | L3084–L3087 | `GET /projects/{id}/tabs` → `{tabs, default_tab, is_default, lock_version, can_manage}`；`PUT` 整份取代 `{lock_version, tabs, default_tab}`（新自訂分頁省略 id）、`Idempotency-Key`；`POST /tabs/reset {lock_version}`；`GET /setting-events?cursor=`（`manage_project`；`pr_hidden`、`pr_unhidden`、以 `pr_ids` 執行的 `github_content_revoked` 只給系統管理員，專案管理者**直接不含**） |
| PUT 交易 | L3090 | `operation_requests` → principal／session `FOR SHARE` → 專案 `FOR SHARE`（active）→ `project_tab_settings` `FOR UPDATE` → 比 `lock_version`（不符 `conflict`＋最新設定）→ R-T1–R-T5 → UPDATE、`lock_version+1` → 事件 |
| 未知 id | L3091 | 內建以外的未知 id → `invalid_value` |
| 議題頁框架 | L3095–L3098 | 看不到 issue 不顯示專案框架（`not_found` 同形狀） |

**測試**（L3104–L3120）

| # | 情境 → 必須成立 |
|---|---|
| T-PT1 | 新建專案 → 同交易建出設定列，五個內建、預設 `timeline`、`is_default = true` |
| T-PT2 | 少一個內建／同一內建兩次 → `builtin_missing`／`invalid_value`，設定不變 |
| T-PT3 | 11 個自訂 → `tab_limit` |
| T-PT4 | 「本週」「本週 」、「Bugs」「bugs」 → `tab_title_taken` |
| T-PT5 | AUTH 裡 `project:WEB status:open`、`project:AUTH project:WEB`、沒寫 project → 前兩者 `tab_query_scope`；第三個存成 `project:AUTH …` |
| T-PT6 | Bob（`own`）開 Alice 建的篩選分頁 → 只看到自己看得到的；沒有總數 |
| T-PT7 | `http://…`、`https://u:p@x.com`、`javascript:…`、自己的 origin、3000 字 → 全部 `tab_url_invalid` |
| T-PT8 | 預設設成隱藏或 link → `tab_default_invalid` |
| T-PT9 | 兩管理者同時改（改過／從沒改過／一改一還原）→ 後送的 `conflict`＋最新設定 |
| T-PT10 | 沒有 `manage_project` 的 PUT → `forbidden_op`；GET `can_manage = false` |
| T-PT11 | bearer 呼叫 → `human_session_required`（見 B1） |
| T-PT12 | 未知 id、已刪除分頁的 id → `invalid_value` |
| T-PT13 | `/issue/AUTH-999`（不存在）與私有看不到的 → 畫面相同（後端：兩者 `not_found` 同形狀） |
| T-PT14 | 同 key 重送 PUT → 回第一次結果，`lock_version` 只加一次 |
| T-PT15 | 專案 closed → GET 可、PUT `forbidden_op` |
| T-PT16 | 只改 `default_tab` → 事件 before／after 的 `default_tab` 不同 |
| T-PT17 | Alice（看得到私有 AUTH-50）隱藏只對到 AUTH-50 的 PR；Bob（AUTH `manage_project`、看不到）讀設定歷史 → 沒有這筆；cursor 與筆數不因它改變形狀；系統管理員看得到 |

### 1.8 GitHub 連動（§19，U45）

**範圍與連動方式**（L3126–L3140）：GitHub App（不是個人 token）；權限只讀（Pull requests、Checks、Commit statuses、Contents、Metadata）；訂閱 `pull_request`、`pull_request_review`、`check_run`、`check_suite`、`status`、`installation`、`installation_repositories`、`repository`。兩層把關：系統管理員標「可連動」、專案管理者從清單挑。App 私鑰與 webhook secret 放設定檔／環境變數，**不進 DB、不進日誌**；installation token 只在記憶體、到期前更新。不做 GHES／GitLab、不寫回 GitHub、不自動改 issue 狀態、不進搜尋。

**DDL**（L3144–L3255，摘要；完整照契約）：

| 表 | 鍵與重點欄位 |
|---|---|
| `github_installations` | `gh_installation_id bigint UNIQUE`、`account_login`、`state IN (active, suspended, deleted)` |
| `github_repos` | `gh_repo_id bigint UNIQUE`、`installation_id`（NULL＝App 已不能存取）、`full_name`、`is_private`、`linkable DEFAULT false`、`state IN (accessible, inaccessible)` |
| `project_repo_links` | `project_id`、`repo_id`、`linked_by/at`、`unlinked_at/by`（同時 NULL）、`sync_state IN (backfilling, ok, paused)`、`synced_through`；部分唯一索引 `(project_id, repo_id) WHERE unlinked_at IS NULL` |
| `github_pull_requests` | `(repo_id, number) UNIQUE`、`title`（截 256 字）、`head_ref`、`head_sha`、`state IN (open, draft, merged, closed)`、`review_state IN (none, pending, approved, changes_requested)`、`ci_state IN (pending, success, failure)` 或 NULL、`html_url`、`gh_updated_at`、`content_hash`、`row_version` |
| `github_check_runs` | `gh_run_id PK`、`repo_id`、`head_sha`、`app_id`、`check_suite_id`、`name`、`status IN (pending, completed)`、`gh_status`、`conclusion`、時間 |
| `github_check_suites` | `gh_suite_id PK`、`repo_id`、`head_sha`、`app_id`、`status`、`conclusion`、`gh_updated_at`、`content_hash`、`row_version` |
| `github_commit_statuses` | PK `(repo_id, head_sha, context)`、`gh_status_id`、`state IN (pending, success, failure, error)` |
| `project_pr_hidden` | PK `(project_id, pr_id)`、`hidden_by/at` |
| `pr_issue_refs` | PK `(pr_id, issue_id)`、`project_id`、`link_id`（讀取時 join 確認有效）、`sources text[]`、`first_seen`；`issue_id` ON DELETE CASCADE |
| `github_deliveries` | `delivery_id uuid PK`（X-GitHub-Delivery）、`event`、`received_at`、`processed_at`、`result`（不存 payload）；保留 7 天 |
| `github_event_jobs` | `delivery_id PK FK`、`payload jsonb`（處理完設 NULL）、`attempts`、`next_at` |

**規則**

| # | 行 | 要點 |
|---|---|---|
| R-G1 | L3261 | 正則 `\b([A-Z][A-Z0-9_]{1,9})-([1-9][0-9]{0,8})\b`，大小寫敏感；掃標題、head 分支名（`/`、`_` 視為分隔）、commit 訊息（最多前 250 個）；key 要存在且 issue 的專案**目前連動**這個 repo（見 B28） |
| R-G2 | L3262 | 只認連動的 repo；解除連動立即刪 refs；讀取端 join `project_repo_links (id = ref.link_id AND unlinked_at IS NULL)` 再擋一次 |
| R-G3 | L3263 | 能看 issue 才看得到 PR；不透露數量；`project_pr_hidden` 依 issue 所屬專案套用；人與 MCP 都套 |
| R-G4 | L3264 | 不寫 issue、不寫 `issue_events`、不 bump `lock_version`、不觸發 reindex |
| R-G5 | L3265 | PR 已合併且 issue 未結案 → 提示（第一版取非 closed 的全部）；不自動轉 |
| R-G6 | L3266 | review：每位 reviewer 取最新；有 CHANGES_REQUESTED → `changes_requested`；否則有 APPROVED → `approved`；否則有待回覆 request → `pending`；否則 `none` |
| R-G7 | L3267 | CI（只看目前 `head_sha`）：有 suite 的 run 用 suite 的結論；沒 suite 的 run 各算一個；status 每 context 一筆；`failure/timed_out/cancelled/action_required/startup_failure`、status `failure/error` → failure；否則任何 pending → pending；否則至少一個 success → success；沒結果 → NULL；`neutral`、`skipped` 不影響；**不自己推定重跑** |
| R-G8 | L3268 | 亂序：PR 與 suite 用快照列規則：`gh_updated_at` 晚 → 採用；早 → 忽略；相同但 hash 不同 → 排 API 重抓；重抓開始記 `row_version`，提交前 `FOR UPDATE` 比對，變了就依時間規則判斷、仍分不出就再抓，最多 3 次，之後 `refetch_gave_up`；suite 可合法 completed → pending；check_run 的 completed 不被非 completed 蓋回；同 run 兩個不同 completed 結論 → 重抓；status 取 `gh_status_id` 大的；`head_sha` 改變不刪舊列 |
| R-G9 | L3269 | 專案最多 20 個 repo；開發卡最多 20 個 PR（open → draft → 其他，再 `gh_updated_at` 新到舊），超過標「還有更多」 |

**webhook 與同步**（L3273–L3285）

1. `POST /integrations/github/webhook`：不經 session、不要 CSRF；原始 body 上限 25 MB（超過 413）；HMAC-SHA256 對 `X-Hub-Signature-256` **常數時間**比較；不符 401、不寫資料、不記 payload。
2. 同一交易 `INSERT github_deliveries … ON CONFLICT DO NOTHING`＋`INSERT github_event_jobs`；提交成功 202；delivery 已存在 200；交易失敗 500。
3. worker（「沿用 D11 的 worker 程序」，見 B22）`FOR UPDATE SKIP LOCKED` 取 job，一事件一交易；失敗 `attempts+1`、指數退避、最多 5 次；成功或最終失敗 `payload = NULL`、寫 `result`。
4. 分派：installation 類只更新 installations／repos（不需要連動）；`repositories_removed`、suspend、deleted → repo `inaccessible`。PR／review／check／status 類：installation active 且 repo accessible，否則 `ignored`；`SELECT … FROM project_repo_links WHERE repo_id = $1 AND unlinked_at IS NULL ORDER BY id FOR SHARE`，沒有 → `ignored`、不存 PR；→ upsert（R-G8）→ 重算 refs（只為鎖住的連動所屬專案，`link_id` 填它）→ 重算彙總。
5. 解除連動：連動列 `FOR UPDATE` → `unlinked_at` → `DELETE FROM pr_issue_refs WHERE link_id = $1`；回填、對帳、重抓走同一段程式與同一個鎖。
6. 回填：連動時抓 open／draft 全部＋最近 30 天關閉的 PR 與它們的 checks，完成 `sync_state = ok`；每 15 分鐘對 open PR 對帳；rate limit → `paused`，依 reset 時間恢復。
7. 補送：每 10 分鐘 `GET /app/hook/deliveries`（最近 3 天），回應非 2xx 且 `guid` 不在 `github_deliveries` 的 → `POST /app/hook/deliveries/{id}/attempts`。
8. 全量對帳：每小時；installation 清單、各 installation 的 repo、每個有效連動 repo：(a) `updated` 晚於 `synced_through − 10 分鐘` 的**所有** PR 完整分頁；(b) 本地 open／draft 但不在 (a) 也不在遠端 open 清單的逐筆重抓；(c) 這些 PR 目前 head 的 suites、checks、statuses；(d) 本地 CI pending（30 天內）與 7 天內合併或關閉的 PR 每次重抓 CI。**全部分頁成功**才推進 `synced_through` 到這次開始的時間。
9. installation suspend／刪除、repo 移出 → `inaccessible`，連動保留、資料停在最後狀態。

**權限、API、錯誤碼**（L3289–L3319）

| 方法 | 路徑 | 誰 | 要點 |
|---|---|---|---|
| GET | `/projects/{id}/repos` | 專案可見 | `full_name`、`is_private`、`state`、`sync_state`、最後同步時間 |
| GET | `/projects/{id}/repos/linkable` | `manage_project` | 開放、可存取、還沒連 |
| POST | `/projects/{id}/repos` | `manage_project`、專案 active | `{repo_id}`、`Idempotency-Key`；鎖序：`operation_requests` → principal／session `FOR SHARE` → 專案 `FOR SHARE` → **`project_tab_settings` 該列 `FOR UPDATE`**（專案設定鎖）→ `github_repos` `FOR SHARE`（重驗 linkable、accessible）→ 計數（R-G9）→ INSERT → `repo_linked` 事件；回 `backfilling` |
| DELETE | `/projects/{id}/repos/{link_id}` | `manage_project`（admin 也可）、專案非 archived | 同交易刪本專案 issue 的 refs；不看 repo 是否 linkable／accessible |
| GET | `/issues/{key}/dev` | 能看 issue | R-G9 |
| GET | `/projects/{id}/dev?filter=all|ci_failure|review|merged_open&cursor=` | 專案可見 | 依 issue 分組、只含看得到的；**不回總數** |
| POST／DELETE | `/projects/{id}/dev/prs/{pr_id}/hide` | `manage_project` | PR 要有至少一筆對到本專案**呼叫者看得到**的 issue 的 ref（經有效連動），否則 `not_found`；同交易寫事件；已是目標狀態 → 成功、不寫事件 |
| POST | `/projects/{id}/dev/revoke` | `manage_project` 或 admin，**step-up** | `{link_id}` 或 `{pr_ids}`；專案 `FOR UPDATE`、bump epoch → 解除或隱藏 → 衍生副本照 §7.5 → `github_content_revoked` 事件（不存原文）（L3324） |
| GET／PUT | `/admin/github` | 系統管理員 | installation 清單、repo 的 `linkable` |
| MCP | `get` 的 `include: ["dev"]` | agent | 回 `dev: {prs: [{repo, number, title, url, state, review_state, ci_state, updated_at}], truncated}`；`describe` 說明唯讀投影、回 `synced_at`；agent 不能連動、不能隱藏（L3310） |

錯誤碼：`repo_not_linkable`（422；不存在、未開放、不可存取三者同形狀）、`repo_already_linked`（409）、`repo_link_limit`（422）、webhook 簽章不符 401 不回 JSON 細節。保存：repo 沒有任何連動後 30 天清 PR、checks；deliveries 7 天（L3327）。

**測試**（L3333–L3368，原文要點）

| # | 情境 → 必須成立 |
|---|---|
| T-G1 | 簽章錯、缺 header、body 改一個 byte → 401；`github_deliveries` 沒有新列 |
| T-G2 | 同 delivery id 兩次 → 第二次 200、不重複處理 |
| T-G3 | 標題「修 AUTH-123」、分支 `fix/AUTH-91-x`、commit「WEB-7 refactor」，repo 只連 AUTH → refs AUTH-123、AUTH-91；WEB-7 忽略 |
| T-G4 | `auth-123`、`XAUTH-123`、`AUTH-0123`、`AUTH-123a` → 都不對到 |
| T-G5 | 同 repo 連 AUTH 與 WEB，PR 寫 AUTH-1、WEB-2 → 各自看到；解除 WEB 只刪 WEB-2 的 ref |
| T-G6 | check 亂序：新的 success 先到、舊的 in_progress 後到 → success |
| T-G7 | PR 推新 commit → 舊 sha 的 checks 保留不參與彙總；新 head 沒結果 → NULL，有未完成 → pending |
| T-G8 | A approve、B request changes、B 再 approve → `approved` |
| T-G9 | Bob 看不到私有 AUTH-50 → 開發分頁與 `/issues/AUTH-50/dev` 都沒有；不透露數量 |
| T-G10 | 連未開放的私有 repo、不存在的 id → 都 `repo_not_linkable`，形狀相同 |
| T-G11 | App 被移出 repo → 連動保留、標無法存取與最後時間；新 webhook `ignored` |
| T-G12 | PR 合併、issue 仍處理中 → 提示；issue 狀態、`lock_version`、`issue_events` 沒變 |
| T-G13 | 回填遇 rate limit → `paused`，reset 後自動繼續 |
| T-G14 | agent `get(include:["dev"])`，issue 在委派範圍外 → `not_found` 形狀，不回 dev |
| T-G15 | repo 改名 → 數字 id 對到同列，`full_name` 更新 |
| T-G16 | 日誌與錯誤訊息不含 webhook secret、installation token、payload 原文 |
| T-G17 | worker 鎖住連動建 ref 時解除連動；反順序也測 → 兩種最後都沒有 ref；讀取端不顯示 |
| T-G18 | 同 PR 對到 AUTH-1、WEB-2，AUTH 隱藏 → AUTH-1 不顯示、WEB-2 照常；MCP 同 |
| T-G19 | 兩個 App 都有 `test`，一 failure、一較新 success → failure；同 workflow 重跑成功（suite 結論更新）→ success |
| T-G20 | 兩快照 `updated_at` 相同內容不同，較新的先到 → 第二個不覆寫、排重抓；重抓後是目前狀態 |
| T-G21 | delivery 交易提交前程序中斷、GitHub 不再送 → 10 分鐘內補送找到並 redeliver；補送也失敗時每小時對帳修正；不會有「有 delivery、沒 job」 |
| T-G22 | 首次安裝（3 repo）、`repositories_removed`、suspend 後延遲到達的 `check_run` → 前兩者更新清單；最後 `ignored` |
| T-G23 | repo `inaccessible` 或 `linkable` 被關 → 仍能解除連動、隱藏、撤除 |
| T-G24 | 撤除某 PR → 專案 epoch bump，讀過該專案的脈絡 `context_unavailable`；提案內容照 §7.5 遮蔽；事件不含標題（見 B14） |
| T-G25 | 重抓到舊狀態 A 的同時較新 webhook 寫入 B → `row_version` 變了，不覆寫 B |
| T-G26 | 同 App 兩個 workflow 都有 `test`，一 failure、一較晚 success → 兩個 suite 各自算 → failure |
| T-G27 | 已有 19 個連動，兩管理者同時新增 → 一成功、一 `repo_link_limit` |
| T-G28 | 刪除有 PR ref 的 issue → 刪除成功，refs 一起刪（見 B15） |
| T-G29 | 有 `manage_project` 但看不到 AUTH-50 的人 hide／revoke 只對到 AUTH-50 的 PR；對同時對到 WEB-2 與 AUTH-50 的 PR 做 AUTH 的 hide → 都 `not_found` |
| T-G30 | 兩次對帳之間 PR 建立並合併，webhook 與補送都失敗 → 下次對帳抓到，refs 與提示出現 |
| T-G31 | 對帳分頁一半遇 rate limit → `synced_through` 不推進；恢復後從原水位重跑 |
| T-G32 | 一個 run 是 `waiting`、其他 success → 存成 pending，CI pending；webhook 與對帳結果相同 |
| T-G33 | 同 workflow 兩個 job 都叫 `test`，一 failure、一較晚 success → suite 結論 failure → failure |
| T-G34 | 已合併 PR 的 CI 仍 pending、`updated` 在水位之前；CI 完成但 webhook、補送都失敗 → 下次對帳因 (d) 抓到 |
| T-G35 | 同 suite：重跑 success（較新）先到、舊 failure 後到；另測對帳重抓舊狀態時較新 webhook 先提交 → 都 success |
| T-G36 | suite 從 completed（failure）因重跑變 in_progress 且較新 → 採用，CI 回 pending |

---

## 2. 對照現況：已有與缺少

| 需求 | 已有（檔案：函式） | 缺少 |
|---|---|---|
| 列表 `review_modes`、diff | `changesets::list`（L2473；`bucket`、`state`；固定 `LIST_LIMIT = 200`；回 title、targets、最近執行狀態）；`validate_in`（L2614） | `group` 參數、cursor（`cursor.rs` 可用）；每列 diff（欄位**顯示名稱**、前後值、同名者附帳號）；`review_modes` 計算（validation、警告、操作白名單）；`evidence_changed` 警告（`citations::verify_one` L509 用 **owner 範圍**驗；目前 `warnings` 永遠空，L2720） |
| `review_mode: "queue"` | `ApproveInput.review_mode`（L326）；fingerprint 含 review_mode（L3211）；DB CHECK | `accept()` 拿掉「只收 detail」（L3397），改成：`detail` 照舊、`queue` 在交易 A 跑資格判定（不合 → `review_outdated` 409，交易 rollback）、`revise_and_apply` 只能由新端點寫（直接送 → `invalid_request`） |
| 步驟 13a | `apply_tx`（L3506）在 `simulate` 之後、步驟 14 之前有位置（L3654） | decision 的 `review_mode` 讀回（`load_exec` 沒帶）；用**執行者（owner）**範圍重驗 evidence；失格 → domain error `review_outdated`，走既有交易 C 記 failed |
| revision-previews | `owner_revise`（L3044，建 owner_revise 版本、鎖脈絡與 R、來源＝目前 R）；衝突三欄已在 `conflict` details | 端點與 service；「逐欄選擇 → 完整修訂文件」的組裝（B11）；token（建議 chacha20poly1305 封裝，金鑰在 `server_secrets` 新名稱 `preview_key`；TTL 可設定，B11）；token 綁定內容的計算（目前 lock_version、epoch、ceiling、expiry、修訂內容摘要＝`proposal_hash` 同一套正規化） |
| revise-and-apply | `owner_revise` 的鎖序與 `accept` 的 decision＋execution 寫法都在 | 新 action `revise_and_apply`（`operation_requests.action`）；交易 A 先查冪等再驗 token（L2096 ②）；目標 issues `FOR SHARE` 重讀；同交易 INSERT 版本＋decision＋execution；之後重用 approve 的 B／C 迴圈（建議把 approve_and_apply 的 B／C 段抽成 `run_accepted(exec_id)`） |
| agent 出口過濾 | `filter_for_agent`、`agent_view`、`agent_get/list/validate/operation`（L2197–L2300）；s6_mcp.rs 有部分覆蓋 | 依 T-M9…T-M20 的編號補測試；T-M20 的「逐位元相同」要把 `request_id`、`observed_at` 以外的欄位全比（含 JSON 欄位順序） |
| §8.9 剩下的配額 | `quota::reserve_proposal`（每委派 60／h＋每 agent 未結案 100）、`agent_write_log` | `validate` 的計數（`agent_write_log.kind` CHECK 要加 `validate`）；同委派同時 2（可比照控制列 NOWAIT，或 advisory lock 2 槽）；payload 768 KiB（MCP 原始 JSON 長度）、正規化 512 KiB；每人、全組織總上限（值未定，B 列 S6 A7 延續） |
| undo（issue） | 表都在；`issue_field_origins` 每次寫都更新；`execution_field_changes` 只在 `record_changes = true`（agent 寫入）時寫；`execution_relation_changes` 記 created | service：`undo_preview(execution)`、`undo(execution, choices)`；鎖序 L2368；逐欄條件 L2331＋L2889；`add_note` 撤回（**沒有 DDL**，B4）；`transition`／`create_issue` 的選擇型 undo；undo 自己 `record_changes = true`（才能「回復一筆 undo」）；來源快照遞移寫 `command_execution_source_projects`；`reverts_execution_id`；`ErrorCode::UndoNotApplicable`；30 天清除（`spawn_sweeper` 加一條） |
| 事故回復 | `agents::suspend_context`（manual） | 凍結清單（`at ≥ HH:MM` 的 execution，依 issue 新到舊）；每筆各自冪等鍵；需要選擇的列出來不執行；端點（B3） |
| 活動頁 | `command_executions`、`changeset_executions`、`agent_write_log`（audience_blocked）、`issue_events` 的 actor／origin | 查詢 service（直接寫入、送審、被擋、undo；篩時間、issue；每筆 diff＝`execution_field_changes`）；「被擋」的資料來源（B8）；端點（B3） |
| 首頁摘要卡、暫停提示 | `list_contexts` 有狀態 | 「今天改了 N 張」（依目前可見 issue 去重；版本另列，L2927）；「可停用」的儲存（B9）；端點（B3） |
| 版本 undo | `version_field_origins`、`execution_version_changes`（versions.rs L1110、L2105） | undo service（L2759 鎖序）；`create_version` → 軟刪除（重用 `versions::delete` 的 R5 邏輯）；`update_version` 逐欄；`in_use` 理由（B7） |
| 時程視圖 | `versions::stats`、`health`、`today()`（L280）、`Thresholds` 讀取（L376） | `timeline` service 與端點；預設範圍演算；每泳道 cursor（`cursor.rs` 的 server 端 handle）；`versions` 參數；`include_closed` |
| 健康度門檻端點 | 表與欄都在（0007） | `GET/PUT /projects/{id}/schedule-settings`、`GET/PUT /org/schedule-settings`；`operation_requests` 冪等（新 action）；`lock_version` 比對；`inherit_org = true` 時兩欄必須省略 |
| `manage_project` | 無（`perm.rs` 權限清單 L31–L47；0007 `role_permissions_known` CHECK） | perm 常數、CHECK 擴充、seed 的 Manager 角色；migration 給既有 Manager 時會觸發全專案 epoch bump（B17） |
| 專案分頁 | 無 | 0015；建立專案時同交易建列（目前沒有建立專案的 API，只有 seed 與 SQL；比照 0007 用 trigger，B20）；R-T1–R-T5 驗證；R-T3 的查詢語法（**不存在**，B2）；R-T4 用 WHATWG URL（`url` crate，§3.10）；端點 4 個；設定歷史的權限過濾 |
| webhook | 無；`api_guard`（api.rs L274）對 bearer 一律 `unauthenticated`、非 GET 要 CSRF 與 Origin | 端點要掛在 guard 之外（B21）；`DefaultBodyLimit` 25 MB；HMAC（`hmac` crate，§3.10）；同交易 delivery＋job |
| worker | `main.rs` 只有 `spawn_indexer`（500 ms 迴圈）、`spawn_field_indexer`（60 s）、`spawn_sweeper`（600 s）；**沒有通用的 job 佇列**（契約說的「D11 worker」就是 index 迴圈） | `spawn_github_worker`（只在 `serve`，stdio 不跑）；`run_jobs_once`、`backfill_once`、`reconcile_once(kind)`、`redeliver_once` 都要能在測試裡直接呼叫 |
| GitHub API client | 無；Cargo.lock 有 hyper 1.12、hyper-util 0.1.21（axum 帶進來）但沒有 TLS | HTTPS client、App JWT（RS256）、installation token 快取、分頁（Link header）、rate limit 判斷；base URL 可設定（測試指向本機 mock） |
| REST／MCP 開發資訊 | `issue_view::get`／`get_many`（L368、L417）的 `include` 機制 | `include: ["dev"]`、`describe` 的說明與 `synced_at`；registry 項目；REST 9 條 |
| 錯誤碼 | `error.rs` 有 `review_outdated`、`version_in_use` 等 | `undo_not_applicable`、`repo_not_linkable`、`repo_already_linked`、`repo_link_limit`；`invalid_value` 的新 `details.kind`（`builtin_missing`、`tab_*`） |
| OpenAPI／TS 型別 | `openapi.rs` | 每個新端點與 DTO 都要進 `api-spec` 產出（SPA 要用） |

**會被 S6b 影響的既有測試**

- `tests/s5_changesets.rs`、`tests/s6_mcp.rs` 裡送 `review_mode` 非 `detail` 預期 `invalid_request` 的斷言（若有）要改。
- 0015 若把 `manage_project` 塞進既有 Manager 角色，會觸發 0010 的 `role_permissions` trigger，bump 全部專案 epoch；migration 在測試 DB 是新建的，影響不大，但已部署的資料會讓所有 agent 脈絡失效（B17）。
- `ProposeResult`／`ChangesetSummary` 加欄位會改 OpenAPI 與 `types.ts`，`tests/s2_api.rs` 若比對產出要更新。

---

## 3. 建議的實作順序

### S6b.1 待審佇列與 `review_mode: "queue"`

- 模組：`changesets.rs`（新函式 `queue_eligibility(conn, viewer_scope, version) -> Result<Eligibility>`，交易 A 與 13a 共用；`list` 改版）；`citations.rs` 的 `verify_one` 以 owner／執行者範圍呼叫。
- 列表：
  - 參數加 `group`（uuid）與 `cursor`（`cursor.rs` 的 server 端 handle，排序 `(created_at DESC, id DESC)`）。
  - 每列加 `version`、`proposal_hash`（給 SPA 勾選綁定）、`diff[]`（`{target{key,subject}, field, label, before, after}`，人名附帳號）、`evidence[]`、`warnings[]`、`validation{status, checked_at}`、`review_modes: ["detail"]` 或 `["detail","queue"]`。
  - 只對 `state = proposed` 的列算 validation 與資格；validation 會模擬，每頁上限建議降到 50（陷阱 P1）。
- 資格（同一個函式）：validation `passed`；`warnings` 空（`evidence_changed`）；ops 只有 `update_fields`（不含 subject、description、is_private，B10）與至多一個 `transition`；不能有 `add_note`、`create_issue`、`duplicate_of`、版本命令；diff 在列表完整顯示（不含長文）。
- `accept()`：`queue` → 鎖住 changeset 之後、寫 decision 之前跑資格判定；失格 → `review_outdated`（`details.reasons`），整個交易 A rollback（`operation_requests` 也不留）。
- 13a：`apply_tx` 在 `simulate` 之後、步驟 14 之前，若 decision 的 `review_mode = queue` 就再判一次（evidence 以 `who` 的範圍驗）；失格回 `review_outdated`，交易 B rollback、交易 C 記 failed。
- 測試（新檔 `tests/s6b_queue.rs`）：T-U1（note 與 description 各一）、T-U2、T-U3（過期 session 的那一份 `unauthenticated` 且零列；前 3 份原 key 查詢不變）、T-U4、T-U9（`faults::arm("approve.after_a", Block(gate))`，在屏障內改另一張 issue 的 description）。另補：資格白名單的表格測試；`evidence_changed` 只出現在 owner 範圍讀到的狀態改變時。

### S6b.2 `revision-previews` 與 `revise-and-apply`

- 模組：`changesets.rs`（`preview_revision`、`revise_and_apply`）；`cursor.rs` 旁新增 `seal.rs`（或擴充 cursor）提供 `seal(name, claims)`／`open(name, token)`，金鑰 `server_secrets.name = 'preview_key'`。
- 預覽輸入（契約沒定，B11，建議）：`{version, choices: {"<op_id>/<field>": "current"|"proposed"}}`；server 用目前值與提案值組出**完整**新文件（含沒衝突的 op、note、evidence），跑 validate（不寫入、不拿 issue 列鎖），回 `{proposal, validation, revision_review_token, expires_at}`。
- token claims：owner、changeset_id、source version、source hash、修訂內容的正規化 hash、每個目標的 `lock_version` 與 `field_values` 摘要、ceiling hash、R 的 epoch 向量、期限、`exp`。
- `revise-and-apply` 交易 A：認證 → `operation_requests` 查詢（`revise_and_apply`，fingerprint＝來源 version＋`expected_current_version`＋修訂內容 hash＋token 摘要）→ 命中就跳到 B／C → 未命中才驗 token（過期、owner 或 changeset 不符 → `review_outdated` 或 `invalid_request`，B11）→ 照 L2096 ③ 鎖序（ChangeSet `FOR UPDATE` → 脈絡 `FOR SHARE` → principals／session／delegation → projects → issues `FOR SHARE`）→ 重算承諾，不符 `review_outdated`（409，全部 rollback）→ INSERT 版本（`owner_revise`、來源＝目前 R）＋decision（`revise_and_apply`）＋queued execution → COMMIT → 共用 B／C。
- 故障點：`revise.after_a`（T-U11 用）。token TTL 在 `test-support` 下可調（T-U11 要等它過期）。
- 測試（`tests/s6b_revise.rs`）：T-U5、T-U6（另一份 ChangeSet、另一個 owner 各一）、T-U7（三種列在同一交易：用屏障在 A 提交前觀察另一連線看不到任何一列）、T-U10（三表筆數前後相同）、T-U11、T-U12。

### S6b.3 agent 出口回歸與 S6 剩下的配額

- 先不改程式，照 §1.3 一條一條寫測試（`tests/s6b_agent_exits.rs`，用 `s6_world` 的 MCP client）；失敗的再修 `filter_for_agent`。
- T-M20：兩個 DB（或同 DB 兩組資料）跑同一個腳本，把回應裡的 `request_id`、`observed_at`、uuid 以外的部分逐位元比較；uuid 用對應表正規化。
- T-M13：選一個 REST 與 MCP 都會出的錯（B13）。
- 配額：migration `0013_quota_validate.sql`：`agent_write_log.kind` CHECK 加 `validate`；validate 每委派 120／h；同委派同時 2；payload 768 KiB（MCP 工具入口量原始 JSON）、正規化 512 KiB（`commands::parse` 之後）；冪等命中不扣。
- 測試：上面四項各一；`rate_limited` 帶 `retry_after_ms`。

### S6b.4 issue 的 undo、事故回復、活動頁、摘要卡

- migration `0014_undo.sql`：
  - `issue_notes` 加 `retracted_at timestamptz`、`retracted_by_execution_id uuid REFERENCES execution_refs(id)`（B4，契約沒有 DDL）；`issue_events.kind` CHECK 加 `note_retracted`（或沿用 `fields_changed`，B4）。
  - （若 B9 決定存在 server）`user_preferences(user_id, key, value)`。
- 模組：新 `undo.rs`。
  - `preview(conn, who, execution_ref) -> UndoPreview`：每欄 `{field, label, before, after, current, restorable, reason}`；duplicate、note、transition 的選項；`expired`（> 30 天）。
  - `apply(conn, who, operation_id, execution_ref, choices)`：鎖序 L2368（session／principal → 來源快照專案 ∪ 目標專案 UUID 序 `FOR SHARE` → issues `FOR UPDATE` → versions（7b）→ 之後才讀 `execution_field_changes`、`issue_field_origins`）；子集合跑 `commands::evaluate`（`Mode::Direct`，human_direct）；`Attribution.record_changes = true`；寫 `command_executions.reverts_execution_id`；來源快照遞移；`reverted_by` 更新（`execution_field_changes.reverted_by` 不是 append-only 表，可 UPDATE）。
  - 撤回 note：`retracted_at` ＋ issue `lock_version + 1`（outbox reindex，公開 note 本文在索引裡）＋事件。
  - duplicate：鎖兩端 issue 後比對 `issue_duplicates.id`（T-D8）。
  - 事故回復：`incident_preview(context or agent, since)` 回凍結清單（execution id、issue key、可自動 undo／需要選擇）與一個清單 token；SPA 對每筆各送 `undo`（各自 Idempotency-Key）。
- 活動頁：`activity(user, agent, from, to, issue?, cursor)`：union `command_executions`（agent_direct 與 reverts）、`changeset_executions`、`agent_write_log` 的 `audience_blocked`（B8），每筆附 diff（遮蔽標記照顯示）。
- 摘要卡：`agent_summary(user, today)`：今天每個 agent 改過的 issue（依目前可見去重）與版本數；被自動暫停的脈絡。
- REST（契約沒列，B3，建議）：`GET /agents/{agent_principal_id}/activity`、`GET /executions/{id}/undo-preview`、`POST /executions/{id}/undo`、`POST /agents/{agent_principal_id}/incident-recovery/preview`、`GET /home/agent-summary`。
- `error.rs`：`UndoNotApplicable`（409 或 422，B3）＋`details.reasons`。
- 30 天清除：`spawn_sweeper` 加 `DELETE FROM execution_field_changes …`？不行：表上的 `before/after` 是 undo 依據，刪了就回 `expired`；建議改成把 `before_value/after_value` 設成標記並保留列（undo 預覽才分得出 `expired`），B3 一併決定。
- 測試（`tests/s6b_undo.rs`）：T-D6、T-D7、T-D8、T-D12；欄位四條件各一；transition 有唯一目標／多個目標；create_issue「關閉為誤建」；undo 的 undo；來源快照遞移（T-D13 的前半，不含遮蔽）；權限（B6）；過期。

### S6b.5 時程視圖、門檻端點、里程碑 undo

- 模組：`versions.rs` 加 `timeline`、`schedule_settings_get/put`、`org_schedule_settings_get/put`；`undo.rs` 加版本分支。
- 時程：`today` 在查詢開始算一次；預設範圍照 L2849–L2856；`versions=` 可列 id 或 `none`；每泳道 200＋`next_cursor`；只用 `load_scope`；`no_dates` 不畫只計數。
- 門檻：PUT 的冪等 action `schedule_settings_update`；`lock_version` 比對；`inherit_org = true` 時兩欄出現就 `invalid_request`；org PUT 要 `is_admin`。
- 版本 undo：鎖序 L2759；`create_version` → 軟刪除（重用 `versions::delete` 的看得到的引用判斷，回 `in_use`，B7）；`update_version` 逐欄看 `version_field_origins`；不看 agent 憑證、委派、配額（T-V35）。
- issue undo 的版本條件：`fixed_version_id` 舊值要 open 且未刪；日期還原後 R4。
- 測試（`tests/s6b_schedule.rs`）：T-V9、T-V9b、T-V10（整合層）、T-V11、T-V12、T-V28、T-V29、T-V33、T-V34、T-V35；時程的 400 天、`from > to`、只帶一端。

### S6b.6 專案分頁

- migration `0015_project_tabs.sql`：
  - `role_permissions_known` CHECK 加 `manage_project`（先 DROP 再 ADD）。
  - `project_tab_settings`、`project_setting_events`（DDL 照 L3022–L3046）；`project_setting_events` 的 `kind` CHECK 一次含 §19 的 5 種，0016 就不用改。
  - 既有專案補列、`projects` AFTER INSERT trigger 建列（比照 0007 的 `project_schedule_settings_row`；預設 tabs 寫成 SQL 常數，B20）。
  - 既有 Manager 角色加 `manage_project`：會 bump 全部 epoch（B17），先問使用者。
- 模組：新 `project_tabs.rs`；`perm.rs` 加 `MANAGE_PROJECT`；`seed.rs` 給 Manager。
- 驗證：title 用 Rust `str::trim()`（Unicode White_Space）→ PG `normalize(NFC)`、`casefold`（同 versions.rs L850）；控制字元檢查；url 用 `url::Url::parse` 後比 scheme、`username()`／`password()`、長度、origin；query 依 B2 的決定。
- 設定歷史：`pr_hidden`、`pr_unhidden`、`pr_ids` 的 `github_content_revoked` 對非系統管理員**在 SQL 條件裡**排除（cursor 形狀不變，T-PT17）。
- 測試（`tests/s6b_tabs.rs`）：T-PT1…T-PT16（T-PT11 依 B1 的決定；T-PT13 用 `GET /issues/{key}` 的 `not_found` 形狀）。

### S6b.7 GitHub：資料模型、webhook 收件、worker、正規化與彙總（不連網）

- migration `0016_github.sql`：§19.3 的 11 張表照抄（L3144–L3255）；`github_deliveries` 7 天、PR／checks 30 天的清除由 sweeper 做。
- 模組：新目錄 `src/github/`：
  - `webhook.rs`：axum handler 掛在 `api_guard` **之外**（B21）；`DefaultBodyLimit::max(25 MiB)`；先讀原始 bytes 算 HMAC（`hmac::Hmac<Sha256>`，`verify_slice` 常數時間），通過才 `serde_json::from_slice`；同交易 delivery＋job；202／200／401／413／500。
  - `jobs.rs`：`run_jobs_once(app, api) -> usize`：`SELECT … FROM github_event_jobs WHERE next_at <= now() AND payload IS NOT NULL ORDER BY next_at FOR UPDATE SKIP LOCKED LIMIT 1`；一事件一交易；失敗退避（2^attempts 秒、上限 5 次）。
  - `model.rs`：webhook payload 與 API 回應共用的正規化（PR state：`draft` 優先於 `open`；`merged_at` 非空 → `merged`；check status 正規化；`content_hash`＝正規化欄位的 sha256）；`upsert_pr`、`upsert_suite`、`upsert_run`、`upsert_status`（R-G8，全部先 `FOR UPDATE` 該列）。
  - `refs.rs`：R-G1 掃描（標題、分支、commit 訊息）；只為已鎖住的有效連動所屬專案建 ref；`issue_keys` 對 key → issue（只讀，不鎖 issue，R-G4）。
  - `rollup.rs`：R-G6、R-G7（純函式，表格驅動測試）。
- worker 在這一步只處理 webhook 帶來的內容；PR 的 commit 訊息與 review 列表需要 API（S6b.8），這一步先用 trait `GithubApi` 的假實作。
- 測試（`tests/s6b_github_ingest.rs`）：T-G1、T-G2、T-G3、T-G4、T-G5（refs 部分）、T-G6、T-G7、T-G8、T-G19、T-G20（重抓用假 API）、T-G22、T-G25（屏障）、T-G26、T-G32、T-G33、T-G35、T-G36；R-G1 的單元測試另含 `_` 分隔（B28）。

### S6b.8 GitHub：API client、回填、對帳、補送、rate limit

- 模組：`src/github/api.rs`：
  - `trait GithubApi`（`app_jwt`、`installation_token`、`list_pulls`、`get_pull`、`list_pull_commits`、`list_reviews`、`list_requested_reviewers`、`check_suites_for_ref`、`check_runs_for_ref`、`statuses_for_ref`、`get_check_suite`、`list_installations`、`list_installation_repos`、`list_hook_deliveries`、`redeliver`）。
  - 實作 `HttpGithub { base: Url, http: Client, app_id, key: RsaKeyPair, tokens: Mutex<HashMap<installation, (token, exp)>> }`。
  - App JWT：header `{"alg":"RS256","typ":"JWT"}`、claims `{iat: now−60, exp: now+540, iss: app_id}`，base64url（既有 `base64`）＋RS256 簽章（§3.10）。
  - installation token：`POST /app/installations/{id}/access_tokens`；記憶體快取，到期前 5 分鐘更新；不寫 DB、不進 log。
  - 分頁：`Link: <…>; rel="next"`；`per_page=100`；PR commits 最多 250。
  - rate limit：`403`／`429` 且 `x-ratelimit-remaining: 0` 或 `retry-after` → `RateLimited { reset_at }`；回填與對帳把連動設 `paused` 並記 `next_at`。
- 設定：`OXLEDGER_GITHUB_APP_ID`、`OXLEDGER_GITHUB_PRIVATE_KEY_FILE`、`OXLEDGER_GITHUB_WEBHOOK_SECRET`、`OXLEDGER_GITHUB_API_URL`（預設 `https://api.github.com`；`http://` 只在 `test-support` 下允許）。沒設 App 時整個 GitHub 功能停用（webhook 回 404 或 503，B21）。
- 背景：`spawn_github_worker`（serve 才跑）：每 1 s `run_jobs_once`；連動 `backfilling` 的跑 `backfill_once`；每 15 分鐘 open PR 對帳；每 10 分鐘 `redeliver_once`；每小時 `reconcile_full_once`（水位規則 L3283）。每個都是可單獨呼叫的函式，測試不靠時間。
- **沒有網路、沒有真 GitHub 的測試方式**：
  - mock：測試內用 axum 起 `127.0.0.1:0` 的假 GitHub（`tests/github_mock/mod.rs`），狀態放在 `Arc<Mutex<MockState>>`：PR、commits、reviews、suites、runs、statuses、deliveries、rate limit 開關、每個端點的呼叫次數；`OXLEDGER_GITHUB_API_URL` 指向它（plain http，不碰 TLS）。
  - JWT：mock 檢查 `Authorization: Bearer <jwt>` 的 header、`iss`、`exp − iat ≤ 600`，並用測試公鑰驗 RS256 簽章；installation token 由 mock 發，之後的 API 呼叫檢查 `Authorization: token …`。
  - 測試用 RSA 金鑰：`tests/fixtures/github-test-app.pem`（2048-bit PKCS#1，用本機 `openssl genrsa` 產一次，檔頭註明只供測試；陷阱 P15）與對應公鑰 DER。
  - webhook：測試用固定 secret 算 `X-Hub-Signature-256 = sha256=<hex(HMAC(secret, body))>`；直接呼叫 router（`tower::ServiceExt::oneshot`，既有 dev 依賴）。
  - 時間：`synced_through`、退避、30／7 天、10 分鐘補送都用參數化的「現在」（`clock_timestamp()` 換成可注入的 `now` 參數或 test-support 下的偏移），不 sleep。
- 測試（`tests/s6b_github_sync.rs`）：T-G13、T-G15、T-G21（webhook 交易注入失敗 → mock 的 deliveries 列出失敗的 guid → `redeliver_once` 呼叫 attempts → mock 重送到 router）、T-G30、T-G31、T-G34；JWT 簽章能被 mock 驗過；token 快取到期更新；T-G16（log 擷取：`tracing` 測試 subscriber 收集輸出，搜尋 secret 與 token 字串）。

### S6b.9 GitHub：連動 REST、開發分頁、隱藏與撤除、admin、MCP

- 模組：`src/github/links.rs`（連動、解除、hide、revoke）、`src/github/read.rs`（`issue_dev`、`project_dev`，所有查詢 join 有效連動、套 `project_pr_hidden`、只含 `load_scope` 看得到的 issue）、`api.rs` 9 條路由、`registry.rs` 的 `get` 加 `include: ["dev"]`、`issue_view.rs` 加 `dev` 區塊、`describe` 說明與 `synced_at`。
- 鎖序：POST repos 照 L3302（`project_tab_settings` 當專案設定鎖）；DELETE 連動 `FOR UPDATE`；revoke：專案 `FOR UPDATE` bump epoch → 連動或 PR → 事件（衍生副本待 S7，B14）；step-up 重用 `auth` 的 reauth。
- `error.rs` 加三個 repo 錯誤碼。
- 測試（`tests/s6b_github_read.rs`）：T-G9、T-G10、T-G11、T-G12、T-G14、T-G16（錯誤訊息部分）、T-G17（屏障：worker 持有連動 `FOR SHARE` 時解除連動；反之）、T-G18、T-G23、T-G24（epoch bump 與脈絡失效部分）、T-G27（並行：兩個 POST 同時，靠 `project_tab_settings FOR UPDATE` 序列化）、T-G28（B15）、T-G29、T-PT17。

### 3.10 可能需要的新 crate（**只列，不下載、不安裝**；加入前請使用者確認）

| crate | 用途 | 現況 | 維護者與判斷依據（非中國） |
|---|---|---|---|
| `hmac` 0.13 | webhook HMAC-SHA256；常數時間 `verify_slice` | **已在 Cargo.lock**（sqlx-postgres、hkdf 帶進來），版本對得上既有 `sha2` 0.11；改成直接依賴不需要新下載 | RustCrypto 組織（GitHub `RustCrypto/MACs`；維護者 Artyom Pavlov〔俄羅斯〕、Tony Arcieri〔美國〕） |
| `url` 2.5 | R-T4 的 WHATWG URL 正規化與 origin 比較 | **已在 Cargo.lock**（sqlx-core 帶進來） | servo 組織（GitHub `servo/rust-url`，Mozilla 出身的專案） |
| `regex` 1 | R-G1 卡號比對（也可以手寫掃描器，不加依賴） | **已在 Cargo.lock**（tantivy 帶進來） | rust-lang 組織（GitHub `rust-lang/regex`，Andrew Gallant〔美國〕） |
| `hyper-util`（啟用 `client-legacy`、`http1`、`tokio` feature） | HTTP client | **已在 Cargo.lock**（axum 帶進來），只是多開 feature；確定不需要新下載要等 `cargo` 實際解析 | hyperium 組織（Sean McArthur〔美國〕） |
| `hyper-rustls` | hyper 的 TLS connector | **新下載** | rustls 組織（GitHub `rustls/hyper-rustls`） |
| `rustls`（`default-features = false`，用 `ring` provider） | TLS | **新下載** | rustls 組織（Joe Birr-Pixton〔英國〕、Dirkjan Ochtman〔荷蘭〕、Daniel McCarney〔加拿大〕；ISRG Prossimo 資助） |
| `webpki-roots` 或 `rustls-native-certs` | 信任根（前者內嵌 Mozilla CA 清單，後者讀 Windows 憑證庫） | **新下載** | rustls 組織 |
| `ring` | RS256 簽 App JWT（`RsaKeyPair::from_der` 吃 PKCS#1、`RSA_PKCS1_SHA256`）；也是 rustls 的 crypto provider | **新下載**（含 C／組語，Windows 需要 MSVC，不需要 cmake／NASM） | Brian Smith〔美國〕（GitHub `briansmith/ring`） |

**刻意不用的**：
- `jsonwebtoken`：JWT 只需要 header＋claims＋RS256，用 `base64`＋`serde_json`＋`ring` 自己組即可，少一層依賴。
- `rsa`（RustCrypto）：私鑰運算有 Marvin timing 公告（RUSTSEC-2023-0071），簽 App JWT 正是私鑰運算。
- `reqwest`：會帶進更多 crate；hyper-util＋hyper-rustls 足夠。
- `aws-lc-rs`（rustls 0.23 的預設 provider）：Windows 建置要 cmake 與 NASM。
- PEM 解析：GitHub 給的是 `-----BEGIN RSA PRIVATE KEY-----`（PKCS#1），用既有 `base64` 去頭尾解碼即可；或用 rustls 帶進來的 `rustls-pki-types` 的 PEM 解析。

國籍判斷的依據是 GitHub 組織頁與維護者個人頁；加入前建議使用者用 `cargo owner --list <crate>`（需要連網）再確認一次。**S6b.1–S6b.7 都不需要新下載**；只有 S6b.8 的 HTTPS 與 RS256 需要。若想把風險隔開，可以把 GitHub client 放在 cargo feature `github` 後面，預設不編。

---

## 4. 契約的疑義與矛盾

| # | 行 | 內容與建議解法 |
|---|---|---|
| B1 | L3114 vs L481、L2951 | T-PT11 要 bearer 呼叫 `/projects/{id}/tabs` 回 `human_session_required`；但 §4.2 寫「帶 bearer 呼叫 `/api/v1` 一律 `unauthenticated`」（`api_guard` 已這樣做，api.rs L288），T-V15 也寫 `unauthenticated`。**建議**：HTTP 層照 L481 回 `unauthenticated`；service 層另測 `human_session_required`（同 S1 把 T-A1 拆兩層的做法）。T-PT11 改寫 |
| B2 | L3065、L3108 | R-T3 要「用設計 03 的搜尋語法」解析 `project:WEB status:open`，但設計 03 沒有文字查詢語法：`search` 是結構化參數（`q`、`project`、`filter` AST），程式裡也沒有 `key:value` 解析器（§17.15 L2912 的 `version:` 語法同樣沒定義）。**建議**二選一：(a) 分頁存結構化查詢 `{q, filter}`，server 把 `project_id eq 本專案` 合進 AST（「沒寫就補」「寫別的就拒」照樣成立）；(b) 先在 03 定一個最小語法（`project:`、`status:`、`tracker:`、`assignee:`、自由文字）。要使用者決定，(a) 工作量小很多 |
| B3 | L2425–L2429、L2372 vs L431–L481 | undo 預覽、執行、事故回復、活動頁、首頁摘要卡都沒有 REST 端點；`undo_not_applicable` 的 HTTP 狀態也沒給（L2402 只寫「SPA 錯誤」）。**建議**端點：`GET /agents/{id}/activity`、`GET /executions/{id}/undo-preview`、`POST /executions/{id}/undo`（Idempotency-Key）、`POST /agents/{id}/incident-recovery/preview`（回凍結清單，SPA 逐筆送 undo）、`GET /home/agent-summary`；`undo_not_applicable` 用 409 |
| B4 | L2326 | `add_note` 的 undo 是「撤回」：一般讀者看到「已由 alice 撤回」、本文隱藏、事件保留。`issue_notes` 沒有撤回欄位，`issue_events.kind` 也沒有對應種類。撤回是否 bump epoch（不是 `is_private` 改變，依 L362 應該不用）、是否 reindex（公開 note 本文在索引裡，要）也沒寫。**建議**：0014 加 `retracted_at`、`retracted_by_execution_id`，事件 kind 加 `note_retracted`，issue `lock_version + 1` 觸發 reindex，不 bump epoch |
| B5 | L2368、L2759 vs L2971 | undo 交易要對來源快照專案「`FOR SHARE` 並比對 epoch」，但沒說和什麼比。若和原 execution 快照的 epoch 比，任何成員變更之後都不能 undo，和 T-V35「委派撤銷、R 擴大後 undo 照常成功」、L2761「不要求原 agent 的委派、脈絡有效」衝突。**建議**：undo 的專案鎖只用來和遮蔽交易排序（§7.5），不比對 epoch 值；只檢查專案 active 與人目前的權限 |
| B6 | L2331、L2425、L2761 | 誰能 undo 沒寫清楚：活動頁在委派者的「Agent 委派頁」，但欄位條件寫「人目前有權修改」、版本寫「執行 undo 的人目前有 `manage_versions`」。其他有權限的人（例如專案管理者）能不能 undo 別人的 agent 的寫入？**建議**：第一切片只限委派者本人（活動頁與事故回復都是委派者的），每欄再照他目前的權限判斷 |
| B7 | L2965 vs L2373 | T-V29 預期 `in_use`，但 §16.7 的 `details.reasons` 列舉沒有 `in_use`（只有 `changed_since`、`value_no_longer_valid`、`permission`、`redacted`、`expired`）。**建議**：列舉加 `in_use` |
| B8 | L2425 vs L933–L939 | 活動頁要列「被擋」，但被擋的請求「失敗不存」；目前只有 `agent_write_log` 的 `audience_blocked`（只有時間，沒有目標與原因）。**建議**：第一切片的「被擋」只列 `audience_blocked` 與熔斷暫停；要更多就另開表，由使用者決定 |
| B9 | L2426 | 摘要卡「可停用」沒有儲存位置（沒有使用者偏好表）。**建議**：存在 SPA 的 localStorage（不需要 DDL）；或 0014 加 `user_preferences` |
| B10 | L2082 | 「短欄位」沒有清單：自訂欄位的長文字型別、`is_private`、`fixed_version_id`、日期算不算？**建議**：白名單 `assigned_to_id`、`priority_id`、`start_date`、`due_date`、`fixed_version_id`、非 text／long_text 的 CF；`is_private` 歸「私有設定」不能勾 |
| B11 | L458、L2095 | `revision-previews` 的輸入形狀沒定（逐欄選擇？完整文件？）；token 的「短效」沒有值；token 不符時回什麼碼也沒寫（T-U6 只寫「拒絕」）。**建議**：輸入 `{version, choices: {"<op_id>/<field>": "current"|"proposed"}}`；TTL 10 分鐘；token 綁的 ChangeSet 或 owner 不符 → `review_outdated`（和「預覽之後又變了」同一個畫面處理），格式壞掉 → `invalid_request` |
| B12 | L1768–L1770 | T-U2、T-U3、T-U4 有一半是前端行為（勾選、標「未送出」、自動取消勾選）。S6b 不做 SPA，只能測 server 那一半；前端那一半要等 SPA 階段補 |
| B13 | L1789 | T-M13 以 `rate_limited` 為例，但 REST 的人類路徑只有登入限流會回 `rate_limited`（MCP 的則是 agent 配額），兩者情境不同。**建議**：用兩邊都會出的 `not_found` 或 `invalid_request` 比對整個錯誤物件；`rate_limited` 另比 `retry_after_ms` 欄位的存在與型別 |
| B14 | L3324、L3368 vs L1806 | 撤除（D12 等級）要同交易處理衍生副本（提案內容、`execution_field_changes` 前後值），那是 S7 的遮蔽交易；T-G24 的「提案內容照 §7.5 遮蔽」在 S6b 做不完（同 S6 的 T-X21、T-X22）。**建議**：S6b 做 epoch bump、解除或隱藏、事件；衍生副本與 T-G24 的那一半移到 S7 |
| B15 | L3364 | T-G28「刪除有 PR ref 的 issue」：第一切片沒有刪除 issue 的命令（describe 的 `unsupported` 有 `delete_issue`），`issue_events` 等表也有 FK 不允許刪。**建議**：只做 DB 層測試（ON DELETE CASCADE 存在），或移到開放刪除的那一版 |
| B16 | L3044 | `project_setting_events.redaction_id` 寫「同 §7.5：管理員可遮蔽」，但沒有對應的 redactions 表與 FK。**建議**：S7 加 `project_setting_event_redactions`（比照 `version_event_redactions`），S6b 只留欄位 |
| B17 | L3077 vs L352 | 新權限 `manage_project`「預設給 Manager 角色」：migration 對既有角色 INSERT `role_permissions` 會觸發 0010 的 trigger，**全部專案 epoch +1，所有 agent 脈絡失效**。`manage_project` 本身和受眾無關。**建議**二選一：(a) 接受並在升級說明寫明「升級後要重建 agent 工作脈絡」；(b) 把 `role_permissions` trigger 改成只對受眾相關的權限（`view_issues`、`view_private_notes`）bump——這是 §3.8 L352 的契約修改，要過一輪審查 |
| B18 | L3305 | `/projects/{id}/dev` 的 `filter=review` 沒定義（`pending`？`changes_requested`？兩者？）；`merged_open` 應該就是 R-G5。**建議**：`review` ＝ `review_state IN ('pending','changes_requested')` 且 PR 未合併未關閉 |
| B19 | L3243、L3282 | `github_deliveries.delivery_id` 是 uuid；GitHub 的 `X-GitHub-Delivery` 是 GUID，實務上是 UUID 格式，但文件沒保證。**建議**：解析失敗 → 400、不入列（記一條 warn，不含 payload） |
| B20 | L3021–L3024 | 「每專案一列，建立專案時同交易 INSERT」，但「預設值由應用層寫入」；程式沒有建立專案的 API（只有 seed 與 SQL）。**建議**：比照 0007 的 `project_schedule_settings_row` 用 AFTER INSERT trigger，預設 tabs 寫成 SQL 常數；應用層只負責驗證 |
| B21 | L479、L3274 | webhook 列在 `/api/v1` 端點表（所以路徑是 `/api/v1/integrations/github/webhook`），但 `api_guard` 對非 GET 要 Origin＋CSRF＋session。「401 不回 JSON 細節」也和 §4.3 的錯誤格式不同。另外沒說 App 沒設定時 webhook 回什麼。**建議**：在 `api_guard` 之前分流（或掛在 v1 router 之外的同一路徑）；401 空 body；沒設定 App → 404 |
| B22 | L3276 | 「沿用 D11 的 worker 程序，新增 job 類型」：程式裡沒有通用 job 機制，D11 的 worker 只是 `spawn_indexer` 迴圈；stdio 程序也會跑 indexer。**建議**：新增 `spawn_github_worker`，只在 `serve` 跑；多個 serve 程序靠 `SKIP LOCKED` 與連動列鎖互斥，排程型工作（對帳、補送）用 `pg_try_advisory_lock` 避免兩個程序同時做 |
| B23 | L3263、L3289–L3294 | R-G3 說 `project_pr_hidden`「依 issue 所屬專案套用」，hide 端點要 PR 有對到「本專案呼叫者看得到」的 issue；但 `GET /projects/{id}/dev` 是「專案可見」就能呼叫，沒說 `manage_project` 以外的人能不能看到「被隱藏」的 PR 清單（管理者要能取消隱藏）。**建議**：被隱藏的只在 `can_manage` 的人以 `include_hidden=true` 查時出現，且仍只含他看得到的 issue |
| B24 | L2830、L2857 | 時程視圖的 `versions=<id,…|none>`：`none` 是「只看未排版本泳道」還是「不看任何版本泳道」？列 id 時要不要也回「未排版本」？**建議**：`none` ＝ 只回未排版本泳道；列 id ＝ 只回那些版本，不含未排版本；省略 ＝ 全部 |
| B25 | L1582 | §8.9「每人、全組織的總上限」仍沒有值（S6 的 A7 延續）。**建議**：S6b.3 先做機制（可設定、預設關閉），值由使用者決定 |
| B26 | L2373、L2437 | undo 資料「保留 30 天」：刪掉 `execution_field_changes` 列會讓 undo 預覽分不出「過期」和「這次沒改這欄」。**建議**：過期時把 `before/after` 換成過期標記、列保留（或另存 `expired_at`），預覽回 `expired` |
| B27 | L2372 | 事故回復「每筆各自冪等」：一筆 execution 一個 undo 請求，但一張 issue 可能有多筆 execution，要「由新到舊」依序做；若中間一筆失敗，後面（較舊）的那筆的條件通常也會失敗（來源不再是它）。契約沒說要不要停。**建議**：照順序逐筆送，失敗的列出來、其餘照送（結果自然是 `changed_since`），不自動停 |
| B28 | L3261 | R-G1 說分支名的 `/`、`_` 視為分隔，但 key 前綴本身允許 `_`（`[A-Z][A-Z0-9_]{1,9}`）；若先把 `_` 換成分隔，`MY_PROJ-12` 這種專案的分支 `fix/MY_PROJ-12` 就對不到。另外 `\b` 把 `_` 當字元，`fix_AUTH-91` 不加處理也對不到。**建議**：分支名只把 `/` 換成空白，`_` 用「前一個字元不是 `[A-Za-z0-9]`」的邊界判斷；或確認專案 key 實際上不允許 `_`（`issue_keys` 的格式要查） |
| B29 | L3268 | R-G8「PR 與 suite 用同一套規則」，但 webhook 的 `check_suite` payload 的 `updated_at` 與 `GET /check-suites/{id}` 是否一定有值、精度是否相同，沒查證。**建議**：實作前用 GitHub 文件確認欄位，記到附錄 B；缺值時當成「無法比較 → 重抓」 |
| B30 | L1805 vs L1807 | 完成標準只列 T-U、T-M，T-PT、T-G 雖在範圍內但沒有列為門檻；T-D6…T-D14、T-V 的 undo／時程測試在 S8。**建議**：S6b 的門檻＝T-U1…T-U12、T-M9…T-M20、T-PT1…T-PT17、T-G1…T-G36（扣掉 B14、B15 移走的部分），T-D、T-V 先寫但不當門檻 |

---

## 5. 陷阱（寫進 playbook）

| # | 症狀 → 原因 → 修法 |
|---|---|
| P1 | 待審列表變很慢 → 每列都跑 validate（模擬＋鎖外讀取）與 evidence 重驗 → 只對 `proposed` 列算；每頁上限 50；evidence 重驗只在有 evidence 時做 |
| P2 | 交易 A 回 `review_outdated` 之後同 key 重送變成 `idempotency_key_reused` → `operation_requests` 列在失敗的交易 A 裡被留下 → 失格要讓整個交易 A rollback（S5 的「失敗不留列」規則） |
| P3 | 13a 用錯範圍 → 用了 agent 的讀取範圍 → 13a 用**執行者**（owner）範圍驗 evidence（L965）；列表的 `evidence_changed` 也是 owner 範圍 |
| P4 | T-U11 重送時 token 已過期卻回 `review_outdated` → 先驗 token 才查冪等 → 一定先查 `operation_requests`，命中就不看 token（L2096 ②） |
| P5 | 預覽 token 在多個 serve 程序間失效 → 金鑰每個程序自己產生 → 放 `server_secrets`（同 cursor_key），程序啟動時快取；輪替要全部重啟 |
| P6 | undo 的 undo 做不到 → undo execution 是 human_direct，`record_changes` 預設 false，沒寫 `execution_field_changes` → undo 的 `Attribution.record_changes = true` |
| P7 | 撤回 note 後搜尋還找得到本文 → 只改 `issue_notes` 沒碰 issue 列 → 撤回也要 `lock_version + 1` 觸發 outbox |
| P8 | undo 和遮蔽死結 → 持有 issue 鎖之後才去鎖專案 → 先鎖來源快照專案 ∪ 目標專案，再鎖 issue，最後才讀前後值（L2368） |
| P9 | 時程視圖的 `today` 一個回應裡不一致 → 每次計算都 `now()` → 查詢開始算一次（R10），傳下去 |
| P10 | 升級後所有 agent 都 `context_unavailable` → 0015 對既有角色加 `manage_project`，`role_permissions` trigger bump 全部 epoch（B17） |
| P11 | 分頁名稱「本週 」沒被判重複 → PG `btrim` 只去 ASCII 空白 → 用 Rust `str::trim()`（Unicode White_Space）再交給 PG `normalize`／`casefold` |
| P12 | GitHub webhook 一律 403／400 → 掛在 `/api/v1` 之下被 `api_guard` 的 Origin／CSRF 擋掉；或 axum 預設 body 上限 2 MB → 在 guard 之前分流、`DefaultBodyLimit::max(25 << 20)` |
| P13 | HMAC 對不上 → 先 `Json<…>` 解析再重新序列化 → 一定對**原始 bytes** 算；比較用 `verify_slice`（常數時間），不要 `==` |
| P14 | installation token 出現在 log → `tracing` 印了 request headers 或錯誤裡帶 URL query → client 不記 headers；錯誤訊息只留狀態碼與端點路徑；T-G16 用測試 subscriber 掃 |
| P15 | push 被擋或被掃描器警告 → 測試用 RSA 私鑰 commit 進 repo → 檔名與檔頭註明 test-only；推之前問使用者是否要在 secret scanning 加例外，或改成測試時用 `openssl` 產（不可攜） |
| P16 | App JWT 被拒 → 本機時鐘比 GitHub 快，`iat` 在未來 → `iat = now − 60`、`exp = now + 540`（GitHub 上限 10 分鐘） |
| P17 | 對帳漏 PR → 依 `updated` 分頁時中途有 PR 更新，跨頁移位 → 用 `sort=updated&direction=asc` 並從水位往後；全部分頁成功才推進 `synced_through`（T-G31） |
| P18 | 同一 PR 被兩個 worker 寫成舊狀態 → 比較時間前沒先鎖列 → webhook、回填、對帳、重抓共用同一個 `upsert_pr`，第一步 `SELECT … FOR UPDATE` |
| P19 | 解除連動後 ref 又冒出來 → worker 沒在建 ref 前 `FOR SHARE` 有效連動，或讀取端沒 join `unlinked_at IS NULL` → 兩道都要（R-G2、T-G17） |
| P20 | `rustls` 在 Windows 編不過 → 預設 provider 是 aws-lc-rs，要 cmake／NASM → `default-features = false` 加 `ring` |
| P21 | 測試偶發等不到背景同步 → 測試依賴 `spawn_github_worker` 的計時 → 所有背景工作都做成可單獨呼叫的 `*_once`，測試直接呼叫，現在時間用參數注入 |
| P22 | 設定歷史的 cursor 在不同人之間形狀不同（T-PT17）→ 先撈再在 Rust 過濾 → 過濾條件放進 SQL，cursor 只看過濾後的列 |
| P23 | 多個 serve 程序同時跑每小時對帳 → 兩邊都推進水位或重複呼叫 API → 排程型工作先 `pg_try_advisory_lock`，拿不到就跳過這輪 |
