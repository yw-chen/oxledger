# S7 實作清單：D12 遮蔽（契約 02 §10 S7）

> 來源：`docs/design/02-first-slice-contract.md`（以下「L123」都指這份檔案）。程式碼基準：分支 `s7-redaction`，從 S6b（`s6b-review`）開出。
> S7 範圍：L1806「note／事件／提案內容的遮蔽交易（含衍生副本規則）；讀取出口擋舊片段」，完成標準 T-S3…T-S6、T-S8、T-S9。
> 另外併入的：T-X21、T-X22（使用者 2026-11-08 決定從 S6 移來）、T-D9、T-D13、T-D14（S6b.4 註明等 S7）、T-G24 的衍生副本那一半（B14）、`project_setting_event_redactions`（B16）、T-V19（版本事件遮蔽）。
> **只做後端**。疑義列在 §3（C1…）；使用者 2026-11-08 說其他疑點「全部照建議」，本階段照建議做，最終回報列出。

## 1. 契約條文

| 條文 | 行 | 要點 |
|---|---|---|
| §7.5 遮蔽 note | L1141 | 專案列 `FOR UPDATE`、bump `security_epoch` → 鎖 issue 與 note → 管理權限 → `body='[已遮蔽]'`、`redacted_at` → `issue_event_redactions`（不存原文）→ 衍生副本 → reindex |
| §7.5 遮蔽事件 | L1142 | 同上；`payload='"[已遮蔽]"'`、`redaction_id` |
| §7.5 衍生副本 | L1141 | 該專案**所有**提案內容（所有版本）＋任何專案中來源快照含該專案的版本＋它們分組的 `label`：`proposal_contents.state='redacted'`、`label_state='redacted'` |
| §7.5 遮蔽提案內容 | L1145 | body 清掉、版本列保留、永遠不能核准或套用；owner 讀歷史只看到「內容已遮蔽」 |
| §7.5 讀取出口 | L1149 | DB 一提交，讀取出口立即以 DB 狀態擋舊片段；私有 note 本文、`visible=false` 的 CF 值不進共用搜尋文件 |
| §16.7 D12 衍生副本 | L2364 | 來源快照含 P **或**目標 issue 在 P 的所有 execution：`execution_field_changes` 前後值換成遮蔽標記；undo 預覽回 `redacted` |
| §16.7 undo 鎖順序 | L2368 | undo 先 `FOR SHARE` 來源專案∪目標專案，遮蔽 `FOR UPDATE`；兩者有先後 |
| §17.13 版本 | L2892 | `versions.*`、`version_events.payload`、`execution_version_changes` 前後值是可遮蔽位置；遮蔽版本事件同一交易 |
| §19.9 撤除 | L3324 | 撤除的衍生副本照「遮蔽事件」那一列 |
| §18.2 設定事件 | L3044 | `project_setting_events.redaction_id`：管理員可遮蔽 |

測試：T-S3（L1747）、T-S4、T-S5、T-S6、T-S8（L1752）、T-S9、T-X21（L1702）、T-X22、T-D9（L2387）、T-D13、T-D14、T-G24（L3368）、T-V19（L2956）。

## 2. 現況

已經有的：讀取端（`issue_view` note／事件的 `redacted`、`changesets` 的 `content_unavailable`、`versions` 事件 `redacted`、undo 的 `redacted` 理由）、`issue_event_redactions`、`version_event_redactions`、`proposal_contents` 的 trigger（只允許 available → redacted／purged）、propose／revise 的 epoch 比對（§3.8）、undo 的鎖順序（S6b.4）、撤除的 epoch bump（S6b.9）。搜尋索引只放 subject、description，候選回 DB 複驗（service.rs）。

缺的：遮蔽交易本身（四種：note、issue 事件、版本事件、設定事件）、衍生副本掃描、端點、`project_setting_event_redactions`、撤除接上衍生副本。

## 3. 疑義與建議（照建議做）

| # | 疑義 | 建議（採用） |
|---|---|---|
| C1 | 契約沒定遮蔽的端點 | `POST /issues/{key}/notes/{note_id}/redact`、`POST /issues/{key}/events/{seq}/redact`、`POST /versions/{id}/events/{seq}/redact`、`POST /projects/{id}/setting-events/{event_id}/redact`，body `{reason_code, reason}`，要 Idempotency-Key；不開 MCP 工具（agent 不能遮蔽） |
| C2 | 「驗證管理權限」是哪個權限 | 該專案 `manage_project` 或系統管理員；只限人的瀏覽器 session；要 15 分鐘內 step-up（不可逆，同撤除）。設定事件照 §18（T-PT17）只給系統管理員看的那幾種，也只給系統管理員遮蔽 |
| C3 | reason_code 值域 | `personal_data`、`confidential`、`legal`、`other`；reason 1–500 字。reason 是遮蔽者自填，不能檢查是否含原文，畫面提示「不要貼原文」 |
| C4 | 已遮蔽再遮蔽 | 成功、`changed=false`，不寫新紀錄、不 bump epoch |
| C5 | 「該專案的提案」範圍 | `changesets.context_project_id = P`（提案的目的地專案）∪ 任一版本的來源快照含 P；該 changeset 的全部版本一起遮蔽（op 目標只存在 body 裡，目的地就是 context_project_id） |
| C6 | execution 範圍 | command 執行：`command_execution_source_projects ∋ P` 或改到 P 的 issue；changeset 執行：該版本的 `changeset_version_source_projects ∋ P` 或改到 P 的 issue。`execution_version_changes` 同理（版本在 P）。遮蔽標記用 `"[已遮蔽]"`（undo 已認得） |
| C7 | reindex | 不改 issue 列（避免動到 lock_version 與 updated_at）；直接寫一筆 outbox（topic issue），索引照常跟上 |
| C8 | B16 表 | migration 0017：`project_setting_event_redactions`（同 version_event_redactions 的形狀）＋FK；同時把版本與分組名稱掃描用到的欄位加索引 |
| C9 | 封存專案 | 系統管理員可遮蔽；專案管理者在封存專案不能（同 §18 的寫入規則）；關閉的專案可以 |
| C10 | 衍生副本失敗注入 | 加 fault 點 `redact.before_derived`（T-V19 要求注入失敗全部 rollback） |
| C11 | 撤除（B14） | `links::revoke` 在同一交易呼叫同一個衍生副本函式 |

## 4. 子步驟

| 步 | 內容 | 測試 |
|---|---|---|
| S7.1 | 0017、`src/redaction.rs`：四種遮蔽交易、衍生副本、reindex；端點與 openapi | T-S5、T-S6、T-S8、T-V19、設定事件 |
| S7.2 | 撤除接衍生副本；undo 與遮蔽交錯 | T-G24、T-D9、T-D13、T-D14 |
| S7.3 | 跨專案與並行；讀取出口 | T-X21、T-X22、T-S9、T-S3、T-S4 |
