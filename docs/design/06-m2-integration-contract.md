# 設計 06 · M2 整合（產品階段 P4）設計契約

> 狀態：**第二版**，2026-11-12。已處理獨立審查的意見（附錄 C 逐條列出問題、處理與改動章節）。§9.1 的 A 類問題（A1–A6）**還沒有使用者決定**，正文先照建議預設寫，相關段落標〔依建議預設 Ax，待確認〕；B 類照預設寫，列在 §9.2（之後進 `docs/impl/decisions.tsv`）。
> 範圍：研究文件 §8 的 M2 定義——**CommonMark（comrak）、Git webhook、`fixed_by`、wiki、CSV 匯入匯出**；D4：不做 Redmine 匯入器、不做 Textile、不做 REST 相容。另外收進：05 延到 M2、而且被上面五項直接帶出的項目（@提及、工時 CSV、附件〔A2〕、`/agent-contexts*` 相容別名移除）。05 延到 M2 的其他項目（email、品牌、設計交接、內建 AI 角色化…）的落點見 §1.3，建議另開 P5 契約〔A1〕；01 §14.8 排在 M2 的圖件（自由心智圖、BPMN 圖件、圖件版本與連結權限）本文**沒有涵蓋**，落點也在 A1 一起決定。
> 讀者：前端 session 照 §6 做畫面；後端照 §2–§5、§7、§8 做。格式比照[設計 05](05-m1-daily-use-contract.md)；**本文沒寫的共通規則一律沿用 02 與 05**（02 §4.3 錯誤格式、§4.5 分頁、§6.7 驗證順序、§6.8 冪等、§16 D20 政策與 undo、§3.8 受眾；05 §1.4 不變條件、§2.1 CHECK 重建規則、§6 共通狀態元件）。
> SQL 都是**新 migration 的草案**，沒有執行或編譯過（0032 已在 M2.0 實作，見 `migrations/0032_m2_prereq.sql`）。寫作時最新是 `0030_ai_chat.sql`；之後 P2 後端用掉了 `0031_p2_api_gaps.sql`，所以本文的編號**全部順延一號**，從 **0032** 起到 **0037**。
> 修訂（2026-11-13，M2.0）：使用者決定 A1–A6 全照建議（正文「待確認」的標記一律視為已決定）；migration 編號 0031–0036 順延成 0032–0037（原 0031＝現 0032、原 0036＝現 0037，正文與附錄已一併改號）；B-1…B-57 已寫進 `docs/impl/decisions.tsv`。
> 標記：【事實】有來源（附錄 A 或程式路徑）；【推論】判斷，沒有直接來源；【待查證】寫作時沒查到，實作前要查；〔依建議預設 Ax，待確認〕。

## 0. 一頁摘要

M2 要做到：**團隊的文件、程式碼與既有資料能接進 oxledger**——說明與留言能用 Markdown 寫並互相連結、專案有 wiki、GitHub 的 commit 會出現在議題上並可依關鍵字推進狀態、清單能匯出成 CSV 給試算表、舊系統的清單能用 CSV 匯進來；而 agent 透過同一套命令引擎與受眾規則讀寫 wiki。

| # | 關鍵決定 | 理由 |
|---|---|---|
| K1 | **Markdown 在伺服端渲染一次、依觀看者解析參照**：comrak 解析成 AST → 走訪文字節點標出議題 key、`@login`、`[[頁面]]` → 渲染時**不透傳任何 HTML**（原文跳脫成文字）→ ammonia 允許清單再消毒一次（第一階段 HTML，可快取）→ 每次讀取依**觀看者當下的權限**把參照換成連結或保留原文（第二階段）。看不到的議題、使用者、wiki 頁和不存在的**輸出完全相同**：就是作者寫的原文 | 一份消毒器，SPA、通知、匯出共用；存在性 oracle 在渲染層就封住（§3.3） |
| K2 | **舊內容不改格式**：既有說明與留言標 `plain`，照純文字顯示；之後新寫的內容是 `markdown`。改既有說明時才轉成 Markdown | 舊資料裡的 `#`、`*`、`1.` 不該突然變成標題與清單 |
| K3 | **wiki 是專案層物件、沒有逐頁讀取權限**：讀者 W(P)＝專案開了 `wiki` 模組、未封存，持有 `view_wiki_pages` 的人，**不論角色的議題可見範圍**（新函式 `wiki_audience()`，不沿用議題的 `holders()`，§3.12 R-AG1）；歷史讀者 H(P) 另要 `view_wiki_edits`。所有寫入是命令引擎的新 op（`wiki_put`、`wiki_move`、`wiki_delete`、`wiki_restore`、`wiki_protect`），每份文件只有**一個目標頁**；唯一會連帶改到別頁的是 `wiki_delete` 帶 `children: "reparent"`（子頁改上層，同一個 execution；鎖序 R-W10、undo R-W9） | 和 Redmine 一致（Redmine 沒有逐頁讀取權限【事實】）；「一份文件一個目標」照 02 §1 |
| K4 | **agent 讀 wiki 的條件是 W(P) ⊇ A(P)，寫 wiki 的目的地檢查照 02 §3.8 M9**（W(P) ⊆ ∩A(R)）。所以 agent 只在「wiki 讀者不比議題讀者少」的專案讀得到 wiki；兩者相等時才寫得進去。不需要像工時那樣的 taint 例外 | 不新增受眾一致保證的例外（05 §1.4 只有 A1(c) 一個例外） |
| K5 | **附件做最小版**〔依建議預設 A2，待確認〕：議題、留言、wiki 頁可附檔；以專案為範圍的 sha256 內容定址；圖片只有本站附件會內嵌；SVG 一律下載不內嵌；agent 只看得到中繼資料、不能上傳下載 | Markdown 圖片、wiki、U25 交接的「有版本基準」都要它；研究 §5.5 已定存法 |
| K6 | **Git 只延伸既有 GitHub App**（U45c 不重開）：多訂閱 `push` 事件，記 commit 並用 R-G1 比對卡號，顯示在議題「開發」卡；仍然**只讀、不寫回 GitHub**（U45a） | 02 §19 的 webhook、worker、鎖、對帳都已實作（S6b.7–S6b.9），延伸不重做 |
| K7 | **`fixed_by`**〔依建議預設 A3，待確認〕：專案管理者設定「關鍵字 → 目標狀態」規則（預設關閉）；PR 合併進預設分支、或 commit 推上預設分支，訊息含 `fixes AUTH-123` 時，worker **先用 GitHub API 確認**真的在預設分支上，再以「整合」身分、**用設定者（sponsor）的權限與 workflow** 走命令引擎 `transition`；不符合條件就只提示（U45b 原本的行為） | 推翻 U45b「只提示」要使用者同意；用真人的權限負責，撤權即停 |
| K8 | **CSV 匯出＝畫面上看得到的東西**：從視圖或篩選匯出，欄位只含觀看者讀得到的；公式注入一律跳脫；UTF-8 BOM＋CRLF；上限 10,000 列；寫稽核紀錄。檔案不存在伺服器（沒有新的 D12 副本） | 匯出是資料出口（01 §14.5），但內容和畫面相同 |
| K9 | **CSV 匯入＝逐列送進命令引擎**：上傳 → 偵測編碼（UTF-8／BOM／Big5）→ 對應欄位與值 → 試跑（`validate`，不寫入）→ 執行（每列一份命令文件、各自冪等、逐列成敗）→ 報表；只建新單與工時，不更新既有單；只限人 | U31「L1 規則對匯入一體強制」；不開第二條寫入路徑 |
| K10 | **@提及發站內通知**：新原因 `mentioned`；只通知「提及當下在該專案可指派清單裡（和自動完成同一個集合）、且派送時讀得到那則內容」的人；agent 寫的提及照樣渲染但**不發通知** | 05 A2 把 @提及排到 M2；通知是新的出口，agent 不該能用它叫人 |
| K11 | **D12 涵蓋所有新內容**：wiki 版本與標題、附件（清除檔案本體）、commit 訊息（擴充既有「撤除」）、匯入暫存檔、提及列；渲染快取以內容 hash 為鍵，遮蔽後自然失效 | 01 §16.7、02 §7.5 |
| K12 | **沒有任何新的伺服端出站請求**，除了既有的 GitHub API（固定在 `OXLEDGER_GITHUB_API_URL`）：不抓外部圖片、不做連結預覽、不從網址匯入 | SSRF 面維持為零 |
| K13 | **security_epoch**：migration 不 bump——0032 授權時暫停 `role_permissions_epoch`。**注意這和 0023 不同**：0023 暫停的是 `enabled_modules_epoch`，它的權限授予**刻意** bump 一次；本文選擇不 bump（理由與兩案代價見 §2.1、B-37）。執行期的權限、模組變更沿用既有 trigger；新增 bump 的只有 D12 遮蔽 wiki／附件、撤除 commit | §2.1 |

實作拆成 **10 階段**（§8，M2.0–M2.9）。A 類 6 題待決（§9.1）。

---

## 1. 範圍

### 1.1 做／不做

| 做（M2） | 不做（理由；排到哪） |
|---|---|
| Markdown：議題說明、留言、wiki 頁；CommonMark＋GFM 表格、刪除線、自動連結、工作清單（唯讀）、註腳、標題錨點；議題 key、`@login`、`[[頁面]]` 自動連結；預覽端點 | Textile（D4）；HTML 透傳；數學式、前置 YAML、巨集（Redmine 的 `{{toc}}` 等）；伺服端語法上色（M3 再看）；工時說明、版本說明、自訂欄位的 Markdown（維持純文字，B-4） |
| 舊內容保留 `plain`，改寫時才轉 Markdown | 一次性批次轉換舊內容 |
| Wiki：建立、編輯（`lock_version`）、改名（留轉址）、改上層、刪除（只限人、可還原 30 天）、保護、起始頁、版本歷史、兩版差異、回復到某版、單頁下載 `.md`、頁面索引樹、wiki 搜尋 | 逐頁讀取權限；跨專案搬頁；wiki 關注與通知（B-12）；PDF／HTML 匯出；巨集 |
| 附件〔A2〕：議題、留言、wiki 頁；上傳暫存 24 小時；內容定址；下載時逐次驗權限；本站圖片內嵌 | 從網址上傳；縮圖；掃毒（只留掛點，B-17）；agent 上傳或讀內容；物件儲存（S3 類，之後再加） |
| Git：GitHub App 加訂 `push`；記 commit（作者 login〔GitHub 未驗證〕、推送者 login、時間、第一行）、比對卡號、開發卡顯示、隱藏與撤除（撤除留持久墓碑） | GitHub Enterprise Server、GitLab、Bitbucket、通用 git webhook（U45c）；repo 瀏覽器（研究 §3）；commit 時間記工時（Redmine 的 `@2h`，B-22） |
| `fixed_by`〔A3〕：專案規則、PR 合併（直接 push 到預設分支依 A3 的觸發來源）、API 確認、以 sponsor 權限轉換、失敗時提示 | 寫回 GitHub（U45a）；依 PR 開啟／review 狀態自動改單 |
| CSV 匯出：議題（視圖或篩選）、工時（報表條件） | XLSX、PDF；agent 匯出（MCP 沒有匯出工具） |
| CSV 匯入：議題（只建新單）、工時；編碼偵測、欄位與值對應、試跑、逐列執行、報表、「關閉這次匯入建立的單」 | 更新既有議題（B-27）；使用者、專案、關係、父子的匯入；Redmine 匯入器（D4、U40） |
| @提及通知（`mentioned`） | email（A1：P5） |
| `/agent-contexts*` 相容別名移除（05 B-43 排定 M2） | — |
| — | 自由心智圖圖件、BPMN 圖件的檢視與編寫、圖件版本與連結權限（01 §14.8、研究 04 排在 M2；本文沒有涵蓋，落點由 A1 決定） |

### 1.2 現況與前提

依 `crates/oxledger/src/`（`claude/p2-backend-01df05`，ccc5a6d）與 migrations 0001–0030 盤點：

| 項目 | 現況【事實，程式路徑】 | M2 還缺 | 本文 |
|---|---|---|---|
| GitHub webhook | `github/webhook.rs`：`POST /api/v1/integrations/github/webhook`，不經 session／CSRF；body 上限 25 MB；`X-Hub-Signature-256` 用 `hmac` 的 `verify_slice`（常數時間）；`X-GitHub-Delivery` 不是 UUID 回 400；delivery 與 job 同交易，重複 delivery 回 200 | `push` 事件；webhook 路由的並行上限（load shed） | §3.7 |
| GitHub worker | `github/jobs.rs` 分派 `installation`、`installation_repositories`、`repository`、`pull_request`、`pull_request_review`、`check_run`、`check_suite`、`status`；**其他事件（含 `push`）`ignored`**；`mod.rs` 的 `spawn_worker` 每秒處理 job、15 分鐘對帳 open PR、10 分鐘補送、每小時全量對帳與清除；排程用 advisory lock | `push` 分派、`fixed_by` 確認工作 | §3.7、§3.8 |
| 卡號比對 | `github/refs.rs`：R-G1 正則、來源 `title`／`branch`／`commit`（PR 的 commit 訊息，前 250 個）；`pr_issue_refs` 綁 `link_id` | commit 層級的 ref（不經 PR） | §3.7 |
| commit 本身 | **沒有存**：只在算 PR ref 時讀訊息 | `github_commits`、`commit_issue_refs` | §2.5 |
| `fixed_by`／`fixes` 語法 | **沒有**（D9 提到 `fixes AUTH-123` 語法解析，未實作）；R-G4「不寫 issue」、R-G5 合併提示已實作 | 全部 | §3.8 |
| GitHub 讀取 | `/issues/{key}/dev`、`/projects/{id}/dev`、MCP `get include: ["dev"]`（`issue_view.rs` `INCLUDES`）；hide、revoke（step-up、bump epoch） | commits、fix links 加進同一投影 | §3.7 |
| GitHub 機密 | `OXLEDGER_GITHUB_APP_ID`、`…_PRIVATE_KEY_FILE`、`…_WEBHOOK_SECRET`、`…_API_URL`，不進 DB | 不變 | — |
| Markdown | **沒有渲染器**；Cargo.toml 沒有 Markdown crate；說明與留言是純文字（`issues.description`、`issue_notes.body ≤ 64 KiB`）。05 §6.19 的 AI 對話是**前端**渲染的 Markdown 子集 | 全部 | §3.2–§3.4 |
| wiki | 沒有 | 全部 | §2.3、§3.5 |
| 附件 | 沒有（沒有 `attachments` 表） | 全部〔A2〕 | §2.4、§3.6 |
| CSV | 沒有；`issue_events.origin` 已有 `seed`／`import`（`actor_kind = system`，給種子資料用） | CSV 匯入**不用** `import` origin：它是人經命令引擎的 `human_direct`，用 `command_executions.import_id` 串起來 | §2.7、§3.10 |
| 模組 | `enabled_modules(project_id, module text)`；`time_tracking` 是 0023 加的值（0023 加模組時暫停 `enabled_modules_epoch`，之後的權限授予**刻意** bump 一次）；trigger `enabled_modules_epoch` | `wiki` 模組值 | §2.2 |
| 權限 | `perm.rs` `PERMISSIONS`（19 個）與 0019 的 `role_permissions_known` CHECK 一致；`role_permissions_epoch` trigger | wiki、匯入、匯出權限 | §2.2 |
| 受眾 | `audience.rs`：A(P) 是實際使用者集合，用集合包含比較；`holders()`（第 76 行）一律 join `enabled_modules … module = 'issue_tracking'`，可見範圍分 `Vis::Public`（all、default）、`Vis::All`、`Vis::Any`（含 own） | `wiki_audience()`（W(P)、H(P)）與 wiki 目的地；**不能直接沿用 `holders()`**：議題模組關時算出空集合、只算 all／default 會漏掉 own 角色 | §3.12 R-AG1 |
| D12 | `redaction.rs` 的 `Target`：Note、IssueEvent、VersionEvent、SettingEvent、TimeEntry；`derived()` 清提案內容、群組標籤、undo 前後值、AI 訊息（依專案） | WikiVersion、WikiEvent、Attachment；`derived()` 加 wiki undo 副本、匯入暫存檔、提及列 | §3.13 |
| 通知 | `notifications.reasons` CHECK 列舉 6 個原因；`notify` consumer 逐收件人重驗 | `mentioned` | §3.4 |
| 搜尋 | tantivy，issue 為中心，`Layout` 有版本、藍綠重建（0018） | wiki 用**獨立索引**（§3.14），不動 issue 的 layout；說明與留言改索引 Markdown 抽出的純文字 | §3.14 |
| 專案分頁 | `project_tabs.rs` `BUILTIN` 7 個（`timeline`…`triage`），附加在尾端（R-T1） | `wiki` 分頁 | §2.3 |

### 1.3 U 項目與 05 延到 M2 的項目落點

| 項目 | 本文？ | 落點與理由 |
|---|---|---|
| U17 事故熔斷 | 延伸 | 「同一張 issue 10 分鐘內被 agent 寫超過 10 次」照樣套到**同一 wiki 頁**（§3.12） |
| U18 undo 30 天 | 沿用 | wiki 與附件刪除的還原、匯入的「關閉建立的單」都在 30 天內 |
| U19 agent 不能被 @ | 沿用 | `@claude-code@alice` 不解析成提及（§3.4 R-MN2） |
| U20 配額 | 沿用 | wiki 寫入計入 `agent_write_log` |
| U21、U38 內建 AI | 延伸 | 內建 AI 工具集合加 `wiki_list`、`wiki_get`；寫入照委派政策（§4.4） |
| U24 介面角色 | 延伸（B-30） | PM、設計師的左欄預設捷徑加「Wiki」；工程師加「開發」 |
| U25 設計交接 | **否，P5**〔A1〕 | 交接卡的「有版本基準」依賴附件（A2）；附件做了之後交接卡另案設計 |
| U26 Triage | 延伸 | 匯入建立的單照樣進收件匣，除非選「直接接受」且有 `triage_issues`（§3.10 R-I9）；`fixed_by` 不動 pending 的單（R-F4） |
| U27 視圖順序 | 不影響 | — |
| U28–U30 品牌 | **否，P5**〔A1〕 | U30 的 SVG 規則先套用在附件：SVG 一律下載、不內嵌（§3.6） |
| U31 表單 L1 | 延伸 | 匯入每列、`fixed_by` 的轉換都過 L1 規則（`evaluate()` 同一段）；L3（專案總覽頁）**P5** |
| U32 `validate` | 延伸 | 匯入試跑用同一個 `validate`；執行中規則變了，該列回 `form_rules_changed`，不中斷整批 |
| U33–U35 AI 團隊 | 延伸 | wiki op 進角色範本的可選清單（不進預設，§4.4）；AI 團隊頁活動加 wiki 寫入 |
| U34 防迴圈 | 延伸 | `fixed_by` 的寫入不算人介入、也不算 agent 寫入：不重設也不累加 agent 鏈（R-F10）；M2 仍沒有站內觸發 |
| U36 delegate | 否 | 依賴 U19 放寬 |
| U37 內建 AI 角色化 | **否，P5**〔A1〕 | 要觸發引擎 |
| U39、U41–U43 里程碑 | 延伸 | 匯入可對應 `fixed_version`（名稱比對，只限本專案 open 版本）；版本說明維持純文字（≤255 字） |
| U40 Redmine 匯入 | 不做 | 和 D4 一致；CSV 匯入不是 Redmine 匯入器 |
| U42 組織時區 | 沿用 | 匯入日期、匯出時間戳、wiki 歷史時間一律組織時區 |
| U44 專案分頁 | 延伸 | 新內建分頁 `wiki`（附加在尾端，R-T1）；`fixed_by` 規則在專案設定，`manage_project` |
| U45 GitHub | 延伸／部分推翻 | (a) 只讀不寫回：**維持**；(b) 合併不自動改單：**A3 建議推翻**（改成可設定、預設關閉）；(c) 只支援 GitHub：**維持**；(d) 系統管理員開放 repo：**延伸到 commit 訊息**（同一個 `linkable` 開關） |
| 05：email／SMTP、digest、退訂連結 | **否，P5**〔A1〕 | 不屬研究 §8 的 M2 五項；@提及先只發站內通知 |
| 05：@提及 | **是** | Markdown 帶出（§3.4） |
| 05：工時 CSV 匯出入 | **是** | §3.9、§3.10 |
| 05：替別人加關注、`view_issue_watchers` | 否，P5 | 和 email 一起看通知對象 |
| 05：`view.render`、view session | 否，P5 | 要 headless 瀏覽器 |
| 05：L3 專案總覽頁、個人釘選欄位、「一併遷移」的表單發布模式 | 否，P5 | 和整合無關 |
| 05：字串 `contains` 篩選 | 否，P5 | 要另設計索引；M2 只把 wiki 加進全文搜尋 |
| 05：概念與 issue 標註 | 否，P5 | 同上 |
| 05：AI provider key 存資料庫 | 否，P5 | 維持只從部署設定讀 |
| 05：對 AI 團隊角色發話、交接、automation 觸發 | 否，M3 | 研究 §8 M3 automation |
| 05 B-43：`/agent-contexts*` 相容別名 | **是，移除**（M2.0） | 05 已排定「M2 移除」；升級說明要寫 |

### 1.4 不變條件與本文的套用方式

沿用 05 §1.4 全部條目（含 A1(c) 工時例外、runtime 工作脈絡、D20 政策、寫入回執 `consistent_after`）。新物件逐條套用如下：

| 不變條件 | 本文的套用 |
|---|---|
| **看不到＝不存在**（02 §4.3、§7.4；M1.11 D1–D3 修過的存在性 oracle） | (1) Markdown 參照：看不到與不存在都輸出原文（§3.3）。(2) 所有新路由（wiki、附件、匯入、commit 隱藏、fix rules、匯出）：看不到的專案、議題、頁、附件、匯入，和不存在的回**同一個** `not_found`，去掉 id 後回應相同；判斷順序一律「先可見性、再 precondition、再權限」（照 `commands::parse_for`）。(3) 匯入的值對應：對應不到的使用者、版本、狀態，看不到與不存在回同一個列錯誤碼（`unknown_value`），訊息不帶候選名單以外的名字。(4) 預覽端點與正式渲染用同一段程式。(5) T-M2-90 把 M1.11 的 46 組比對擴充到本文所有新路由與新 op |
| **所有寫入只走命令引擎** | wiki、附件掛載與刪除、匯入每列、`fixed_by` 都是命令文件；REST 只有設定類（wiki 起始頁、fix rules、匯入設定）不是 issue／wiki 內容寫入，照 02 §18 的專案設定模式寫 `project_setting_events`；worker 自動停用 fix 規則也寫 `project_setting_events`，以系統 principal 建 `operation_requests`（R-F8） |
| **agent 寫入**：D20 政策（direct／review／off）、受眾（M8、M9）、鏈上限、配額、暫停 | wiki op 進 `delegation_command_policies`；目的地受眾 W(P)（§3.12）；配額與 U17 熔斷照算；暫停時 `agent_paused`；agent 沒有匯入、匯出、附件、fix rules、wiki 刪除與保護 |
| **D12** | §3.13 逐物件列出 |
| **通知逐收件人重驗**（05 K9） | `mentioned` 照同一套：通知列只存參照，讀取時重驗 |
| **計數只算看得到的** | wiki 頁數、匯入結果的「建立 N 張」只回呼叫者自己建立的；匯出不回總數 |
| **新 op 不自動加進既有委派**（02 §16.3 規則 1） | wiki op 只出現在之後建立或編輯的委派；委派頁顯示「新操作」標記（05 M1.3 的做法） |

---

## 2. DDL 草案

### 2.1 migration 一覽

| 編號 | 內容 | 階段 |
|---|---|---|
| 0032 | 新權限（CHECK 聯集重建）；`issues.description_format`、`issue_notes.body_format`；`issue_event_mentions`；通知原因 `mentioned` 與偏好；`issue_events`、`command_executions` 的 origin 加 `integration`（CHECK 聯集重建）；`command_executions.import_id`（先建欄、FK 在 0037） | M2.0 |
| 0033 | wiki：`wiki_settings`（回填既有專案＋新專案 trigger）、`wiki_pages`、`wiki_page_versions`、`wiki_redirects`、`wiki_events`、`wiki_redactions`、`execution_wiki_changes`；政策、提案 op、`agent_write_log` 的 CHECK 聯集；wiki 索引的 outbox consumer **不在 migration 建**（`wiki@<世代>`，§3.14） | M2.2 |
| 0034 | 附件〔A2〕：`attachment_blobs`（以世代為列）、`uploads`、`upload_daily_usage`、`attachments`；`issue_events_kind_check` 聯集 | M2.4 |
| 0035 | Git：`github_repos.default_branch`、`github_commits`（含 `sender_login`、`redacted_at`）、`commit_issue_refs`、`project_commit_hidden`、`project_commit_revocations`（撤除墓碑）；`project_setting_events_kind_check` 聯集 | M2.5 |
| 0036 | `fixed_by`：`project_fix_rules`、`issue_fix_links` | M2.6 |
| 0037 | 匯入匯出：`imports`、`import_rows`、`export_events`；`command_executions.import_id` 的 FK | M2.7、M2.8 |

**合併順序必須等於編號順序**（05 §2.1）。M2.5–M2.6（Git）可以和 M2.2–M2.4 並行開發，但要提早合併就改號到當時的下一號，並同步改本表與 §8。

**CHECK 重建**：本文各節寫的固定清單是**要新增的值**；重建時一律「`pg_get_constraintdef` 讀出現行值的聯集 ＋ 本文新值」（05 §2.1、附錄 D V12）。受影響的 CHECK：`role_permissions_known`、`notifications.reasons`、`issue_events` 的 origin 與 provenance、`command_executions.origin`、`delegation_command_policies.command_kind`、`delegations_proposal_ops_check`、`changeset_version_ops_kind_check`、`agent_write_log` kind、`issue_events_kind_check`（新值 `attachment_added`、`attachment_removed`，0034）、`project_setting_events_kind_check`（新值 `wiki_start_page`〔0033〕、`commit_hidden`、`commit_unhidden`〔0035〕、`fix_rule_changed`〔0036〕）。`wiki_events.kind` 是 0033 新建的表，不用重建。T-M1-124 的比對擴充成 T-M2-96（程式常數 vs `pg_get_constraintdef`）。

**security_epoch**：
- **migration 不 bump**（B-37）。0032 加權限並授給既有角色時，用 `ALTER TABLE role_permissions DISABLE TRIGGER role_permissions_epoch` → INSERT → `ENABLE`。**更正第一版的說法**：這**不是** 0023 的做法——0023 暫停的是 `enabled_modules_epoch`（只包住加 `time_tracking` 模組那一句），之後的 `role_permissions` 授予**刻意讓 trigger bump 一次**（0023 的註解：權限授予本來就會讓每個專案 bump 一次）。本文刻意和 0023 不同，兩案比較：
  - **不 bump（預設）**。為什麼安全：epoch 的用途是讓「讀過某專案內容的工作脈絡」在那些內容的讀者集合改變後失效。(1) 升級前不存在任何 wiki、附件內容，沒有任何脈絡讀過它們；(2) 0032 的新權限都不是 `view_issues`、也不改角色的 `issues_visibility`，既有議題、留言、工時的讀者集合 A(P) 在升級前後**完全相同**；(3) agent 讀 wiki 的條件（R-AG1）在每次讀取當下用 `wiki_audience()` 計算，不依賴 epoch。代價：這個論證只在「新權限不影響既有內容的讀者」時成立——將來若有 migration 把既有內容改掛到新權限下，就不能照抄；升級測試（§7.9）要斷言升級前後所有專案的 epoch 不變、既有脈絡不會 `context_unavailable`。
  - **照 0023 bump 一次**。代價：每個專案 epoch +1，升級後**所有 agent 工作脈絡**（含內建 AI 的對話）在下一次讀寫時 `context_unavailable`，每個使用者都要重開脈絡；好處是不用上面的論證。
- **`wiki` 模組不自動開**給既有專案（B-9），所以也不用動 `enabled_modules_epoch`。新建專案預設開（新專案沒有既有脈絡）。
- **執行期**：權限、角色、模組、成員變更沿用既有 trigger（都會 bump）；本文新增的 bump 點只有 §3.13 的遮蔽（wiki 版本、wiki 事件、附件）與撤除 commit（沿用 02 §19.9 revoke）。
- `issues.description_format` 等格式欄位、提及列、匯入表不影響讀者集合，不 bump。

**升級說明要求**（M2.9 前寫進 `docs/impl/upgrade-notes.md`）：(1) 0032 授權規則（下節）與「既有內容保留純文字」；(2) wiki 模組既有專案預設關、新專案預設開；(3) GitHub App 要在 GitHub 上**加訂 `push` 事件**（權限不變，`Contents: read` 已有【推論：GitHub 文件說 push 事件需要 Contents 權限；加訂事件是否需要安裝者重新同意，待查證】）；(4) 附件的 `OXLEDGER_FILES_DIR`、備份要和 PostgreSQL 同時間點；(5) `/agent-contexts*` 移除；(6) 匯出上限與 `export_issues` 權限的授予規則（依 A4）；(7) `format` 省略的語意：**更新**既有說明時省略＝保留原格式，M1 的用戶端與 agent 不會把 `plain` 默默轉成 `markdown`（R-M9）；(8) GitHub 的限制：一次推送超過 3 個 tag 或 5000 個分支時 GitHub 不送 push 事件（R-C4）；(9) 附件的配額、同時上傳數與磁碟門檻設定（R-A2）。

### 2.2 0032：權限、格式、提及、整合 origin

```sql
-- 權限（聯集重建；以下只列新值）
--   view_wiki_pages, view_wiki_edits, edit_wiki_pages, rename_wiki_pages,
--   delete_wiki_pages, protect_wiki_pages, manage_wiki,
--   import_issues, import_time_entries, export_issues   -- export_issues 依 A4
-- 都不是 tracker-scoped（role_permissions_tracker_scope 不變）。

-- 既有角色的授予（trigger 暫停，見 §2.1）：
--   view_wiki_pages, view_wiki_edits ← 持有 view_issues 且 visibility IN ('all','default') 的角色（含 Non member、Anonymous）
--     （own 可見範圍的角色不自動授予，B-50；管理者之後加了，W(P) 會比 A(P) 寬：agent 讀得到、寫不進 wiki，R-AG3）
--   edit_wiki_pages                  ← 持有 edit_issues 的角色
--   rename/delete/protect_wiki_pages, manage_wiki, import_issues, import_time_entries ← 持有 manage_project 的角色
--   export_issues（A4 選 (b) 才有）    ← 持有 view_issues 的角色（行為與升級前「沒有匯出」不同，但和選 (a) 等價）

ALTER TABLE issues ADD COLUMN description_format text NOT NULL DEFAULT 'plain'
    CHECK (description_format IN ('plain', 'markdown'));
ALTER TABLE issues ALTER COLUMN description_format SET DEFAULT 'markdown';
ALTER TABLE issue_notes ADD COLUMN body_format text NOT NULL DEFAULT 'plain'
    CHECK (body_format IN ('plain', 'markdown'));
ALTER TABLE issue_notes ALTER COLUMN body_format SET DEFAULT 'markdown';
-- 先以 'plain' 加欄（既有列全部 plain，PG 18 的 ADD COLUMN 常數預設不改寫表），再改預設。

-- 提及（只存 user id，不存文字；D12 遮蔽來源時一起刪）
CREATE TABLE issue_event_mentions (
    event_id uuid NOT NULL REFERENCES issue_events(id),
    user_id  uuid NOT NULL REFERENCES principals(id),
    PRIMARY KEY (event_id, user_id)
);
CREATE TRIGGER issue_event_mentions_user_guard BEFORE INSERT OR UPDATE OF user_id ON issue_event_mentions
    FOR EACH ROW EXECUTE FUNCTION user_column_guard('user_id');   -- 0028 的泛用 guard：只能是 user

-- 通知原因（聯集重建 notifications.reasons，新值 'mentioned'）
ALTER TABLE notification_preferences ADD COLUMN on_mentioned boolean NOT NULL DEFAULT true;

-- 整合寫入（fixed_by，§3.8）：origin 'integration'，actor 是 system principal，
-- on_behalf_of = 規則的 sponsor；一定有 command_execution（走引擎）。
-- issue_events 的 provenance CHECK 聯集重建，加一個分支：
--   OR (origin = 'integration' AND actor_kind = 'system'
--         AND system_operation = 'fixed_by' AND on_behalf_of_id IS NOT NULL
--         AND command_execution_id IS NOT NULL AND operation_request_id IS NOT NULL
--         AND changeset_id IS NULL AND execution_id IS NULL AND proposer_id IS NULL
--         AND approver_id IS NULL AND delegation_id IS NULL)
-- command_executions.origin 聯集加 'integration'，對應的 CHECK 分支同理
-- （on_behalf_of_id NOT NULL、delegation_id NULL；operation_request_id 在該表本來就 NOT NULL）。
-- 系統 principal 當 caller：operation_requests.caller_id = commands::SYSTEM_PRINCIPAL（0008 的 system
--   principal），operation_id = UUIDv5(NS_FIXED_BY, issue_fix_links.id)（operation_id 是 uuid，不能放
--   'fix:{id}' 字串），action = 'command'；UNIQUE (caller_id, operation_id) 保證同一 fix link 只執行一次。

ALTER TABLE command_executions ADD COLUMN import_id uuid;   -- FK 在 0037
CREATE INDEX command_executions_import_idx ON command_executions (import_id) WHERE import_id IS NOT NULL;
```

### 2.3 0033：wiki

```sql
CREATE TABLE wiki_settings (                  -- 每專案一列；建頁與起始頁設定的序列化鎖
    project_id    uuid PRIMARY KEY REFERENCES projects(id),
    start_page_id uuid,                       -- FK 見下；NULL = 用標題 'Wiki' 的頁，再沒有就顯示索引
    page_count    integer NOT NULL DEFAULT 0 CHECK (page_count BETWEEN 0 AND 5000),
    lock_version  bigint NOT NULL DEFAULT 0
);
-- 回填：0033 對每個既有專案 INSERT 一列；新專案用 AFTER INSERT ON projects 的 trigger 建
-- （同 0015 的 project_tab_settings_row）。R-W10 的鎖都假設這列一定存在。

CREATE TABLE wiki_pages (
    id              uuid PRIMARY KEY DEFAULT uuidv7(),
    project_id      uuid NOT NULL REFERENCES projects(id),
    title           text NOT NULL CHECK (char_length(title) BETWEEN 1 AND 100),
    title_key       text NOT NULL,            -- R-W2：空白串→'_' 後 NFKC_Casefold；唯一性用
    parent_id       uuid REFERENCES wiki_pages(id),
    current_version integer NOT NULL CHECK (current_version >= 1),
    lock_version    bigint NOT NULL DEFAULT 0,
    protected       boolean NOT NULL DEFAULT false,
    created_at      timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    deleted_at      timestamptz,
    deleted_by_execution_id uuid REFERENCES execution_refs(id),
    CHECK ((deleted_at IS NULL) = (deleted_by_execution_id IS NULL)),
    CHECK (parent_id IS DISTINCT FROM id),
    UNIQUE (project_id, id)                   -- 給 parent 的同專案複合 FK
);
ALTER TABLE wiki_pages ADD FOREIGN KEY (project_id, parent_id) REFERENCES wiki_pages (project_id, id);
CREATE UNIQUE INDEX wiki_pages_title_uq ON wiki_pages (project_id, title_key) WHERE deleted_at IS NULL;
CREATE INDEX wiki_pages_tree_idx ON wiki_pages (project_id, parent_id) WHERE deleted_at IS NULL;
ALTER TABLE wiki_settings ADD FOREIGN KEY (project_id, start_page_id) REFERENCES wiki_pages (project_id, id);

CREATE TABLE wiki_page_versions (             -- 內容歷史；只增不改（D12 遮蔽例外）
    page_id     uuid NOT NULL REFERENCES wiki_pages(id),
    version     integer NOT NULL CHECK (version >= 1),
    body        text NOT NULL CHECK (octet_length(body) <= 524288),
    body_format text NOT NULL DEFAULT 'markdown' CHECK (body_format = 'markdown'),
    comment     text NOT NULL DEFAULT '' CHECK (char_length(comment) <= 255),
    -- provenance：和 0023 time_entry_events 同一組欄位與 CHECK
    origin text NOT NULL, actor_id uuid NOT NULL REFERENCES principals(id), actor_kind text NOT NULL,
    proposer_id uuid, approver_id uuid, on_behalf_of_id uuid, delegation_id uuid, agent_client_id uuid,
    operation_request_id uuid, command_execution_id uuid, changeset_id uuid, changeset_version integer,
    execution_id uuid, op_id uuid,
    created_at  timestamptz NOT NULL DEFAULT now(),
    redaction_id uuid,                        -- 非 NULL：body 與 comment 都已換成 '[已遮蔽]'
    PRIMARY KEY (page_id, version)
);

CREATE TABLE wiki_redirects (                 -- 改名留下的舊標題
    project_id uuid NOT NULL REFERENCES projects(id),
    title_key  text NOT NULL,
    page_id    uuid NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (project_id, title_key),
    FOREIGN KEY (project_id, page_id) REFERENCES wiki_pages (project_id, id)   -- 轉址不能指到別專案的頁
);

CREATE TABLE wiki_events (                    -- 版本以外的變更；標題在 payload（D12 範圍）
    id      uuid PRIMARY KEY DEFAULT uuidv7(),
    page_id uuid NOT NULL REFERENCES wiki_pages(id),
    seq     bigint NOT NULL CHECK (seq >= 1),
    kind    text NOT NULL CHECK (kind IN ('created', 'renamed', 'moved', 'deleted', 'restored',
                                          'protected', 'unprotected', 'attachment_added', 'attachment_removed')),
    payload jsonb NOT NULL,                   -- renamed: {from, to}；moved: {from_parent, to_parent}
    -- provenance 同上
    at      timestamptz NOT NULL DEFAULT now(),
    redaction_id uuid,
    UNIQUE (page_id, seq)
);

CREATE TABLE wiki_redactions (                -- 誰、何時、為什麼；不存原文（同 issue_event_redactions）
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    page_id uuid NOT NULL REFERENCES wiki_pages(id),
    target text NOT NULL CHECK (target IN ('version', 'event', 'title')),   -- 'version' 同時換掉 body 與 comment
    version integer, event_id uuid,
    reason_code text NOT NULL, reason text NOT NULL,
    redacted_by uuid NOT NULL REFERENCES principals(id),
    redacted_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE execution_wiki_changes (         -- undo 用；保留 30 天；D12 衍生副本（標題）
                                              -- wiki_delete 帶 reparent 時，每個被改上層的子頁也各一列
    execution_id uuid NOT NULL REFERENCES execution_refs(id),
    page_id      uuid NOT NULL REFERENCES wiki_pages(id),
    before_version integer, after_version integer,
    before_title text, after_title text,
    before_parent uuid, after_parent uuid,
    before_deleted boolean, after_deleted boolean,
    before_protected boolean, after_protected boolean,
    reverted_by uuid,
    PRIMARY KEY (execution_id, page_id)
);

-- 聯集重建（新值）：
--   delegation_command_policies.command_kind  + 'wiki_put', 'wiki_move'
--   delegations_proposal_ops_check            + 'wiki_put', 'wiki_move'
--   changeset_version_ops_kind_check          + 'wiki_put', 'wiki_move'
--   agent_write_log kind                      + 'wiki_write'
-- wiki_delete、wiki_restore、wiki_protect 只限人，不進上面三個清單。
-- wiki 索引的 outbox consumer 不在 migration 建：照 0018 的世代機制，服務啟動時建 'wiki@<世代 uuid>'
-- （state bootstrapping → active；藍綠重建時舊世代 retained → retired），§3.14。
```

專案分頁：`project_tabs.rs` `BUILTIN` 尾端加 `("wiki", "Wiki")`，0021 的 `project_tabs_default()` 同步；模組沒開或沒有 `view_wiki_pages` 時這個分頁不出現（和 `dev` 分頁的做法相同）。

### 2.4 0034：附件〔依建議預設 A2，待確認〕

```sql
CREATE TABLE attachment_blobs (               -- 以專案為範圍去重（跨專案去重會洩漏「某檔案存在」，研究 §5.5）
    id           uuid PRIMARY KEY DEFAULT uuidv7(),   -- 一列＝一個「世代」：同內容被遮蔽清除後再上傳，是新的一列（R-A12）
    project_id   uuid NOT NULL REFERENCES projects(id),
    sha256       bytea NOT NULL CHECK (octet_length(sha256) = 32),
    size         bigint NOT NULL CHECK (size BETWEEN 1 AND 104857600),
    sniffed_type text NOT NULL,               -- 只依檔頭判斷：image/png|jpeg|gif|webp，其餘 application/octet-stream
    state        text NOT NULL DEFAULT 'present' CHECK (state IN ('present', 'purged')),
    created_at   timestamptz NOT NULL DEFAULT now(),
    purged_at    timestamptz,
    purge_reason text CHECK (purge_reason IN ('gc', 'redacted')),
    CHECK ((state = 'purged') = (purged_at IS NOT NULL)),
    UNIQUE (project_id, id)
);
-- 去重只在「還在」的世代之間：同專案同內容最多一個 present
CREATE UNIQUE INDEX attachment_blobs_live_uq ON attachment_blobs (project_id, sha256) WHERE state = 'present';

CREATE TABLE uploads (                        -- 暫存：上傳後 24 小時內掛到內容上，否則清除
    id          uuid PRIMARY KEY DEFAULT uuidv7(),
    project_id  uuid NOT NULL REFERENCES projects(id),
    uploader_id uuid NOT NULL REFERENCES principals(id),   -- user_column_guard：只能是人
    blob_id     uuid NOT NULL,
    filename    text NOT NULL CHECK (char_length(filename) BETWEEN 1 AND 255),   -- 禁 Cc、Cf（R-W2 同一組）
    size        bigint NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    expires_at  timestamptz NOT NULL,
    consumed_at timestamptz,
    FOREIGN KEY (project_id, blob_id) REFERENCES attachment_blobs (project_id, id)
);

CREATE TABLE upload_daily_usage (             -- R-A2 每人每日位元組配額；只存數字
    user_id uuid NOT NULL REFERENCES principals(id),
    day     date NOT NULL,                    -- 組織時區（U42）
    bytes   bigint NOT NULL DEFAULT 0 CHECK (bytes >= 0),
    PRIMARY KEY (user_id, day)
);

CREATE TABLE attachments (
    id           uuid PRIMARY KEY DEFAULT uuidv7(),
    project_id   uuid NOT NULL REFERENCES projects(id),
    issue_id     uuid REFERENCES issues(id),
    note_id      uuid,                        -- 掛在某則留言（可見性跟留言，含私有留言）
    wiki_page_id uuid REFERENCES wiki_pages(id),
    wiki_version integer,                     -- 掛上時的頁版本（顯示用）
    blob_id      uuid NOT NULL,
    filename     text NOT NULL CHECK (char_length(filename) BETWEEN 1 AND 255),
    size         bigint NOT NULL,
    content_type text NOT NULL,               -- = blob 的 sniffed_type
    author_id    uuid NOT NULL REFERENCES principals(id),
    created_execution_id uuid NOT NULL REFERENCES execution_refs(id),
    created_at   timestamptz NOT NULL DEFAULT now(),
    deleted_at   timestamptz, deleted_by_execution_id uuid REFERENCES execution_refs(id),
    redacted_at  timestamptz, redaction_id uuid,   -- 逐列標記：遮蔽時同 blob 的每一列都標（R-A12）
    FOREIGN KEY (project_id, blob_id) REFERENCES attachment_blobs (project_id, id),
    FOREIGN KEY (issue_id, note_id) REFERENCES issue_notes (issue_id, id),
    CHECK (num_nonnulls(issue_id, wiki_page_id) = 1),     -- exclusive arc（研究 §5.5）
    CHECK (note_id IS NULL OR issue_id IS NOT NULL),
    CHECK ((wiki_page_id IS NULL) = (wiki_version IS NULL)),
    CHECK ((deleted_at IS NULL) = (deleted_by_execution_id IS NULL)),
    CHECK ((redacted_at IS NULL) = (redaction_id IS NULL))
);
CREATE INDEX attachments_issue_idx ON attachments (issue_id) WHERE issue_id IS NOT NULL;
CREATE INDEX attachments_wiki_idx ON attachments (wiki_page_id) WHERE wiki_page_id IS NOT NULL;
CREATE INDEX attachments_blob_idx ON attachments (blob_id);
-- 掛上、刪除、還原寫 issue_events（issue_events_kind_check 聯集加 'attachment_added'、'attachment_removed'）
-- 或 wiki_events（0033 已含這兩個 kind），payload 只有 attachment id 與檔名。
```

檔案本體放 `$OXLEDGER_FILES_DIR/<project_id>/<blob id 前 2 碼>/<blob id>`（以世代 id 命名，不用 sha256：清除後重新上傳的同內容是另一個檔）；上傳時**邊收邊寫**暫存檔並同時算 sha256（不整份放記憶體，R-A2），`fsync`、`rename`，成功後才在交易裡寫 `attachment_blobs`；同專案已有 `present` 的同內容世代時刪掉暫存檔、沿用那個世代。沒設定 `OXLEDGER_FILES_DIR` 時附件功能關閉（上傳回 `forbidden_op`，`reason = attachments_disabled`；畫面不顯示附件區）。

### 2.5 0035：Git commit

```sql
ALTER TABLE github_repos ADD COLUMN default_branch text;   -- 每個帶 repository 物件的事件都更新；NULL＝還不知道

CREATE TABLE github_commits (
    repo_id      uuid NOT NULL REFERENCES github_repos(id),
    sha          text NOT NULL CHECK (sha ~ '^[0-9a-f]{40}$'),
    message      text NOT NULL CHECK (char_length(message) <= 4096),   -- 截斷；顯示第一行
    author_login text,                          -- commit 的 author.username：**未驗證**（作者欄可任意填）；不存 email、姓名
    sender_login text,                          -- 第一次收到它的那次 push 的 sender.login（GitHub 驗證過的推送者）
    committed_at timestamptz,
    -- 不存 html_url：讀取時由 github_repos.full_name ＋ sha 組 https://github.com/{full_name}/commit/{sha}
    on_default_branch boolean NOT NULL DEFAULT false,
    first_ref    text,                          -- 第一次看到它的分支名（截 255）
    first_seen   timestamptz NOT NULL DEFAULT now(),
    last_seen    timestamptz NOT NULL DEFAULT now(),
    redacted_at  timestamptz,                   -- 撤除後 message = '[已遮蔽]'；upsert 不得覆寫（R-C3）
    PRIMARY KEY (repo_id, sha)
);
-- upsert：ON CONFLICT (repo_id, sha) DO UPDATE SET last_seen = now(),
--   on_default_branch = github_commits.on_default_branch OR EXCLUDED.on_default_branch
--   （message、author_login、sender_login 一律不覆寫；已遮蔽的列 message 保持 '[已遮蔽]'）
CREATE TABLE commit_issue_refs (
    repo_id    uuid NOT NULL,
    sha        text NOT NULL,
    issue_id   uuid NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    project_id uuid NOT NULL,
    link_id    uuid NOT NULL REFERENCES project_repo_links(id),
    first_seen timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (repo_id, sha, issue_id),
    FOREIGN KEY (repo_id, sha) REFERENCES github_commits (repo_id, sha) ON DELETE CASCADE
);
CREATE INDEX commit_issue_refs_issue_idx ON commit_issue_refs (issue_id);
CREATE INDEX commit_issue_refs_link_idx ON commit_issue_refs (link_id);
CREATE TABLE project_commit_hidden (
    project_id uuid NOT NULL REFERENCES projects(id),
    repo_id uuid NOT NULL, sha text NOT NULL,
    hidden_by uuid NOT NULL REFERENCES principals(id),
    hidden_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (project_id, repo_id, sha),
    FOREIGN KEY (repo_id, sha) REFERENCES github_commits (repo_id, sha) ON DELETE CASCADE
);
CREATE TABLE project_commit_revocations (    -- 撤除墓碑：持久，不隨 github_commits 清除而消失（沒有 FK 到 commit）
    project_id uuid NOT NULL REFERENCES projects(id),
    repo_id    uuid NOT NULL REFERENCES github_repos(id),
    sha        text NOT NULL CHECK (sha ~ '^[0-9a-f]{40}$'),
    revoked_by uuid NOT NULL REFERENCES principals(id),
    revoked_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (project_id, repo_id, sha)
);
-- project_setting_events_kind_check 聯集 + 'commit_hidden', 'commit_unhidden'（'fix_rule_changed' 在 0036）
```

### 2.6 0036：`fixed_by`〔依建議預設 A3，待確認〕

```sql
CREATE TABLE project_fix_rules (
    id           uuid PRIMARY KEY DEFAULT uuidv7(),
    project_id   uuid NOT NULL REFERENCES projects(id),
    enabled      boolean NOT NULL DEFAULT false,
    sponsor_id   uuid NOT NULL REFERENCES principals(id),   -- user_column_guard；最後一次啟用或修改的人
    keywords     text[] NOT NULL CHECK (cardinality(keywords) BETWEEN 1 AND 10),   -- 存小寫
    target_status_id uuid NOT NULL REFERENCES issue_statuses(id),
    tracker_ids  uuid[],                        -- NULL＝全部 tracker
    disabled_reason text CHECK (disabled_reason IN ('sponsor_inactive', 'sponsor_lost_permission',
                                                    'status_removed', 'manual')),
    lock_version bigint NOT NULL DEFAULT 0,
    updated_at   timestamptz NOT NULL DEFAULT now()
);
-- 每專案最多 5 條；同專案內關鍵字不重複（交易內檢查，鎖 project_tab_settings 該專案列＝專案設定鎖，同 02 §19.7）
-- project_setting_events_kind_check 聯集 + 'fix_rule_changed'

CREATE TABLE issue_fix_links (
    id          uuid PRIMARY KEY DEFAULT uuidv7(),
    issue_id    uuid NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    project_id  uuid NOT NULL,
    repo_id     uuid NOT NULL REFERENCES github_repos(id),
    pr_id       uuid REFERENCES github_pull_requests(id) ON DELETE CASCADE,
    sha         text,
    link_id     uuid NOT NULL REFERENCES project_repo_links(id),
    rule_id     uuid REFERENCES project_fix_rules(id),    -- NULL＝沒有啟用的規則（只提示）
    keyword     text NOT NULL,
    confirm_head text,                          -- push 來源：這次 push 的 after sha；同一 push 的列共用一次 compare（R-F3）
    state       text NOT NULL CHECK (state IN ('pending', 'applied', 'skipped', 'suggested')),
    reason      text,                           -- skipped／suggested 的原因碼（§3.8 R-F6）
    execution_id uuid REFERENCES execution_refs(id),
    attempts    smallint NOT NULL DEFAULT 0,
    next_at     timestamptz NOT NULL DEFAULT now(),
    detected_at timestamptz NOT NULL DEFAULT now(),
    decided_at  timestamptz,
    CHECK (num_nonnulls(pr_id, sha) = 1),
    CHECK (sha IS NULL OR sha ~ '^[0-9a-f]{40}$')
);
CREATE UNIQUE INDEX issue_fix_links_pr_uq ON issue_fix_links (issue_id, pr_id) WHERE pr_id IS NOT NULL;
CREATE UNIQUE INDEX issue_fix_links_sha_uq ON issue_fix_links (issue_id, repo_id, sha) WHERE sha IS NOT NULL;
CREATE INDEX issue_fix_links_pending_idx ON issue_fix_links (next_at) WHERE state = 'pending';
```

### 2.7 0037：匯入與匯出

```sql
CREATE TABLE imports (
    id           uuid PRIMARY KEY DEFAULT uuidv7(),
    project_id   uuid NOT NULL REFERENCES projects(id),
    kind         text NOT NULL CHECK (kind IN ('issues', 'time_entries')),
    owner_id     uuid NOT NULL REFERENCES principals(id),   -- user_column_guard
    state        text NOT NULL CHECK (state IN ('uploaded', 'ready', 'validating', 'validated',
                                                'running', 'done', 'canceled', 'failed', 'expired')),
    file         bytea,                         -- 原檔 ≤ 10 MB；完成 24 小時後、刪除時、專案遮蔽時設 NULL（D12）
    file_sha256  bytea NOT NULL,
    encoding     text NOT NULL,                 -- 'utf-8' | 'utf-16le' | 'utf-16be' | 'big5'
    encoding_guessed boolean NOT NULL,
    delimiter    text NOT NULL CHECK (delimiter IN (',', ';', E'\t')),
    row_count    integer NOT NULL CHECK (row_count BETWEEN 1 AND 5000),
    settings     jsonb NOT NULL DEFAULT '{}',   -- §3.10：對應、值對應、日期格式、選項（不含檔案內容）
    lock_version bigint NOT NULL DEFAULT 0,
    operation_request_id uuid REFERENCES operation_requests(id),   -- run 的冪等
    created_at   timestamptz NOT NULL DEFAULT now(),
    started_at   timestamptz, finished_at timestamptz, purged_at timestamptz
);
CREATE INDEX imports_owner_idx ON imports (owner_id, created_at DESC);

CREATE TABLE import_rows (                    -- 只存結果，不存儲存格內容
    import_id   uuid NOT NULL REFERENCES imports(id) ON DELETE CASCADE,
    row_no      integer NOT NULL CHECK (row_no >= 1),   -- 檔案中的資料列號（不含標題列）
    state       text NOT NULL CHECK (state IN ('pending', 'ok', 'error', 'skipped')),
    error_code  text,                             -- 錯誤碼與欄位名；不回寫儲存格內容
    error_field text,
    issue_id    uuid REFERENCES issues(id),
    time_entry_id uuid REFERENCES time_entries(id),
    execution_id  uuid REFERENCES execution_refs(id),
    PRIMARY KEY (import_id, row_no)
);
ALTER TABLE command_executions ADD FOREIGN KEY (import_id) REFERENCES imports(id);

CREATE TABLE export_events (                  -- 稽核：誰匯出了什麼範圍；不存內容、不存條件原文
    id          uuid PRIMARY KEY DEFAULT uuidv7(),
    actor_id    uuid NOT NULL REFERENCES principals(id),
    kind        text NOT NULL CHECK (kind IN ('issues', 'time_entries', 'wiki_page')),
    project_id  uuid REFERENCES projects(id),   -- 跨專案匯出為 NULL
    view_id     uuid,
    filter_hash bytea,                          -- 正規化條件的 HMAC-SHA256（金鑰在 server_secrets，用途 'export_filter'）；
                                                -- 不用純 sha256：條件值空間小，可用字典還原受限內容
    columns     text[] NOT NULL DEFAULT '{}',   -- 欄位 key（cf:<id>），不是值
    row_count   integer NOT NULL,
    bytes       bigint NOT NULL DEFAULT 0,      -- 實際送出的位元組（R-E6 的 50 MB 上限）
    truncated   boolean NOT NULL DEFAULT false, -- 串流中途超過位元組上限而中斷
    at          timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX export_events_at_idx ON export_events (at);
```

---
## 3. 規則與 REST API

### 3.1 端點總表

前綴 `/api/v1`；寫入一律 `Idempotency-Key`（02 §6.8）；agent token 對這些 REST 路由一律 401（agent 只走 MCP；05 M1.11 的 HTTP 檢查照樣涵蓋）。

| 方法 | 路徑 | 誰 | 用途 | 章節 |
|---|---|---|---|---|
| POST | `/markdown/preview` | 人 | `{context: {issue}｜{project}｜{wiki_project}, text}` → `{html, format_version, refs_truncated}`；依呼叫者解析參照；每人每分鐘 60 次 | §3.2 |
| GET | `/issues/{key}`（既有） | 人 | 加 `description_format`、`description_html`、`description_by_agent`；`recent_notes[].body_format`、`body_html`；`attachments[]`〔A2〕 | §3.2 |
| GET | `/issues/{key}/notes`（既有） | 人 | 同上 | — |
| GET | `/projects/{id}/wiki` | 人（W） | `{start_page, can: {create, manage}, tree: [{id, title, parent_id, updated_at}]}`（最多 5000 頁，一次回） | §3.5 |
| GET | `/projects/{id}/wiki/pages/{title}` | 人（W） | 頁：`{id, title, body, body_html, version, lock_version, protected, parent, breadcrumbs, children, updated_at, updated_by, attachments, redirected_from, can: {edit, rename, delete, protect, history}}`；標題命中轉址時照常回頁並帶 `redirected_from` | §3.5 |
| GET | `/projects/{id}/wiki/pages/{title}/versions?cursor=` | 人（`view_wiki_edits`） | `[{version, author, origin, comment, size, created_at, redacted}]`，新到舊 | §3.5 |
| GET | `/projects/{id}/wiki/pages/{title}/versions/{n}` | 人（`view_wiki_edits`） | 某版內容與 `body_html` | §3.5 |
| GET | `/projects/{id}/wiki/pages/{title}/diff?from=&to=` | 人（`view_wiki_edits`） | `{hunks: [{old_start, new_start, lines: [{op, text, words?}]}], truncated}` | R-W7 |
| GET | `/projects/{id}/wiki/pages/{title}/export.md` | 人（W；匯出權限依 A4） | 下載目前版本 Markdown；寫 `export_events` | R-W12 |
| GET | `/wiki/search?q=&project=&cursor=` | 人 | wiki 全文搜尋，只含呼叫者讀得到的 wiki（§3.14）；不回總數 | §3.14 |
| GET | `/projects/{id}/wiki/deleted`、`/projects/{id}/wiki/deleted/{page_id}` | 人（`manage_wiki`） | 30 天內刪除的頁清單；單頁刪除時的內容（另有 `view_wiki_edits` 才含版本清單） | R-W16 |
| GET／PUT | `/projects/{id}/wiki/settings` | 人（`manage_wiki`） | `{start_page_id, lock_version}`；寫 `project_setting_events`（`wiki_start_page`） | R-W13 |
| POST | `/commands`（既有） | 人 | 新 op：`wiki_put`、`wiki_move`、`wiki_delete`、`wiki_restore`、`wiki_protect`、`attach`、`delete_attachment`；`create_issue`、`add_note`、`update_fields` 收 `format` 與 `attachments` | §3.11 |
| POST | `/projects/{id}/uploads?filename=` | 人（專案內任一可附檔的權限） | 原始 body（不用 multipart）；`Content-Length` 必填；這條路由個別放寬 body 上限、邊收邊寫暫存檔；大小、配額、同時數、磁碟門檻見 R-A2；匿名不能上傳；回 `{upload_id, size, content_type, expires_at}` | §3.6 |
| DELETE | `/uploads/{id}` | 上傳者本人 | 放棄暫存 | §3.6 |
| GET | `/attachments/{id}` | 人（讀得到容器） | 中繼資料 | §3.6 |
| GET | `/attachments/{id}/content` | 人（讀得到容器） | 檔案本體；標頭見 R-A6 | §3.6 |
| GET | `/issues/{key}/dev`（既有） | 人 | 加 `commits[]`、`fix_links[]` | §3.7、§3.8 |
| GET | `/projects/{id}/dev`（既有） | 人 | `filter` 加 `fix_skipped` | §3.8 |
| POST／DELETE | `/projects/{id}/dev/commits/{repo_id}/{sha}/hide` | 人（`manage_project`） | 在本專案隱藏 commit；可見性前提同 02 §19.7 的 PR hide | §3.7 |
| POST | `/projects/{id}/dev/revoke`（既有） | 人（`manage_project` 或系統管理員，step-up） | 加 `{commits: [{repo_id, sha}]}` | §3.7 |
| GET／PUT | `/projects/{id}/fix-rules` | 人（`manage_project`） | 整份取代 `{rules: [...], lock_version}`；PUT 的人成為所有**有變動或被啟用**的規則的 sponsor | §3.8 |
| POST | `/exports/issues` | 人（A4；匿名不行） | `{view_id}｜{where, project_id?}`、`columns?`、`include_description?` → `text/csv`（逐列串流，R-E6） | §3.9 |
| POST | `/exports/time-entries` | 人（A4；讀得到工時） | 報表條件 → `text/csv` | §3.9 |
| GET | `/admin/export-events?cursor=` | 系統管理員 | 匯出稽核 | R-E8 |
| POST | `/projects/{id}/imports?kind=issues｜time_entries` | 人（`import_issues`／`import_time_entries`） | 原始 body ≤ 10 MB → `{import}`（偵測結果、前 10 列預覽、建議對應） | §3.10 |
| GET | `/projects/{id}/imports` | 人 | 自己在本專案的匯入（新到舊，30 天內） | §3.10 |
| GET | `/imports/{id}` | 擁有者 | 狀態、設定、進度（`done/total`）、計數 | §3.10 |
| PUT | `/imports/{id}/settings` | 擁有者 | `{encoding?, delimiter?, date_format, mapping, value_map, options, lock_version}`；state 回到 `ready` | §3.10 |
| POST | `/imports/{id}/validate` | 擁有者 | 試跑（不寫入）；同一匯入同時只有一個試跑 | R-I8 |
| POST | `/imports/{id}/run` | 擁有者 | 開始執行（`Idempotency-Key`）；state 必須是 `validated` 且設定沒變 | R-I10 |
| POST | `/imports/{id}/cancel` | 擁有者 | 目前這列做完就停 | R-I10 |
| GET | `/imports/{id}/rows?state=error&cursor=` | 擁有者 | 列結果 | §3.10 |
| GET | `/imports/{id}/report.csv` | 擁有者 | `row, status, key, error_code, field`（不含原內容） | §3.10 |
| POST | `/imports/{id}/close-created` | 擁有者（且仍有權轉換） | 「撤銷匯入」 | R-I12 |
| DELETE | `/imports/{id}` | 擁有者 | 刪除暫存檔與設定；已建立的單不受影響 | R-I13 |
| GET／PUT | `/me/notification-preferences`（既有） | 人 | 加 `on_mentioned` | §3.4 |
| — | `/agent-contexts*` | — | **移除**（05 B-43）；改用 `/me/agent-runtimes*`、`/ai-team*` | M2.0 |

「人（W）」＝讀得到該專案 wiki（R-W1）。所有 `{id}` 路由：看不到的與不存在的回同一個 `not_found`。

### 3.2 Markdown 方言與渲染管線

| # | 規則 |
|---|---|
| R-M1 | **方言**：CommonMark（comrak 實作的版本）＋ GFM 擴充：表格、刪除線、自動連結（`https://…`、`www.`）、工作清單（渲染成**停用的** checkbox，唯讀）、註腳；標題錨點與註腳的 id、`href` 一律加**容器前綴**（`h-<容器短 id>-…`、`fn-<容器短 id>-<n>`）：同一頁有說明和多則留言，comrak 預設的 `fn-1` 會互相撞名，也避免和 SPA 元件 id 撞名（用 comrak 選項或 AST 後處理，附錄 B C12）；`[[…]]` wiki 連結（comrak 的 wikilinks 擴充，顯示文字在管線後：`[[頁面\|顯示文字]]`）。不開：數學式、前置 YAML、description list、上標、emoji 短碼、Redmine 巨集。`format_version = 1`；方言或規則改變就 +1（快取鍵的一部分，也回給前端） |
| R-M2 | **不透傳 HTML**：原文裡的 HTML 標籤一律**跳脫成文字**顯示（comrak 的 escape 選項，不是「省略」——讓作者看得到自己寫了什麼）。之後 ammonia 用允許清單再消毒一次（縱深防禦）：標籤 `p br hr h1–h6 strong em del code pre blockquote ul ol li table thead tbody tr th td a img input sup section span div`；屬性只允許管線自己產生的（`href`、`src`、`alt`、`title`、固定值集合的 `class`、`h-` 前綴的 `id`、`data-ox-*`、checkbox 的 `type`／`checked`／`disabled`、表格的 `align`）；URL scheme 只有 `http`、`https`、`mailto` 與本站相對路徑。**本站相對路徑**只接受「單一 `/` 開頭、第二個字元不是 `/` 也不是 `\`」：`//evil.example`、`/\evil.example` 在瀏覽器裡是外部網址，一律當外部連結（R-M3），圖片則不載入（R-M4）。ammonia 另設 `attribute_filter`：`img` 的 `src` 必須**完整**符合 `^/attachments/[0-9a-f-]{36}/content$`，不符就移除整個 `img`；`input` 的 `type` 只能是 `checkbox`（其他值移除整個元素），並強制 `disabled` |
| R-M3 | **連結**：`javascript:`、`data:`、`vbscript:`、`file:` 等 scheme 的連結**去掉連結、保留文字**。外部連結（主機不是本站）加 `target="_blank" rel="noopener noreferrer nofollow ugc"`、`class="ox-ext"`、`data-ox-host="<主機>"`；前端在連結後顯示主機名（小字）。**agent 寫的內容**（留言作者是 agent；說明的有效來源 `issue_field_origins` 是 agent 的 execution；wiki 版本 `actor_kind = agent`；**經 ChangeSet 套用的內容，只要提案來源是 `external_agent` 或 `builtin_ai`，即使核准、套用的是人，也算 agent 內容**——判斷看 execution 的 `proposer_id` 是 agent）再加 05 §6.19 的規則：網址 > 200 字元或查詢字串 > 100 字元**不渲染成連結**，只顯示純文字；其餘外部連結點擊時前端先確認（05 附錄 D V23 的資料外送管道）。人改寫 agent 內容後，新版本算人的內容、不再套嚴格規則：編輯器在原內容是 agent 內容時提示「含 agent 寫入的連結」（§6.2）；人把 agent 內容**複製**到別處不追蹤（B-38） |
| R-M4 | **圖片**：只有本站附件會內嵌——`![說明](attachment:檔名)` 或 `![說明](/attachments/{id}/content)`，而且附件在**這段內容可解析的範圍**內：議題說明只解析**議題層附件**（`note_id IS NULL`）；留言解析**自己的附件＋議題層附件**（不解析別則留言的附件——否則私有留言的附件能被公開內容引用出來）；wiki 頁解析該頁的附件；並且未刪除、未遮蔽、R-A3 判定為圖片；同名多個取最新。其他圖片（外部網址、別的容器的附件、`data:`）**不載入**，渲染成文字連結「外部圖片：〈說明〉（主機）」（點了在新分頁開，走 R-M3）。理由：自動載入外部圖片會把觀看者的 IP、時間送給第三方，也是被注入內容的資料外送管道。A2 不做附件時，全部圖片都走「不載入」 |
| R-M5 | **管線**：(1) 解析成 comrak AST；(2) 走訪**不在** code span、code block、連結文字、自動連結裡的文字節點，切出議題 key（R-RF1）、`@login`（R-MN1），換成佔位節點；wiki 連結節點同樣換成佔位節點；(3) 依 R-M3、R-M4 改寫連結與圖片節點；(4) 渲染成 HTML（escape 模式）；(5) ammonia 消毒 → **第一階段 HTML**。佔位節點渲染成 `<span data-ox-ref="issue｜user｜wiki" data-ox-v="…">原文</span>`：使用者寫不出這種標籤（R-M2 把原文 HTML 跳脫了），ammonia 只對這三個值放行 `data-ox-*`；(6) 讀取時依觀看者做**第二階段**（§3.3），用固定模板產生連結，值一律 HTML 跳脫，不和使用者可控的字串拼接 |
| R-M6 | **快取**：第一階段 HTML 以 `(sha256(原文), format, format_version, 是否 agent 作者, 容器 id)` 為鍵，放記憶體 LRU（預設 10,000 筆、10 分鐘）；附件新增或刪除時清該容器的項目。第二階段不快取（每次依觀看者算）。內容被 D12 遮蔽後原文 hash 不再出現在 DB，舊項目不會再被命中，等 LRU 淘汰 |
| R-M7 | **上限**：渲染輸入 512 KiB；一份文件最多解析 200 個參照，超過的保留原文並回 `refs_truncated: true`。預覽端點的 `text` 上限同各欄位上限 |
| R-M8 | **`plain` 格式**：照純文字顯示——HTML 跳脫、換行變 `<br>`、只做 http(s) 網址自動連結（同 R-M3），**也做**議題 key 與 `@login` 的第二階段解析（舊內容裡的 `AUTH-123` 也能點）；不解析 `[[…]]`、不做其他 Markdown 語法 |
| R-M9 | **格式轉換**：只有「人在 SPA 編輯既有 `plain` 說明」時轉成 `markdown`（編輯器載入原文，預覽讓人看到差異，存檔時 `format: "markdown"`）。留言不能改格式（留言不能編輯，02）。命令文件的 `format`：**建立**（`create_issue`、`add_note`）省略＝`markdown`；**更新**既有說明（`update_fields`）省略＝**保留原格式**——M1 的用戶端與 agent 改說明時不會把 `plain` 默默轉成 `markdown`（寫進升級說明）；要轉格式必須明寫 `format: "markdown"`。agent 只能產生 `markdown` 內容：更新 `plain` 說明時省略 `format` 回 `forbidden_op`（`reason = agent_format`），要明寫 `markdown`（`describe text_format`，§4） |
| R-M10 | **API 形狀**：回應同時給原文（`description`／`body`）與 `*_html`；SPA 只把 `*_html` 欄位放進 `{@html}`（前端 lint：`{@html}` 只接受以 `_html` 結尾的 API 欄位）；MCP 只回原文與 `format` |
| R-M11 | **搜尋與通知摘要**用 Markdown 抽出的純文字（去掉語法、保留連結文字與 code 內容），在第一階段的 AST 上抽；議題索引因此重建一次（§3.14） |
| R-M12 | **CSP**：SPA 回應要有（或補上）`Content-Security-Policy: default-src 'self'; img-src 'self'; script-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'`、`Referrer-Policy: same-origin`。`img-src 'self'` 讓 R-M4 即使前端出錯也載不到外部圖片。SvelteKit 的 inline 樣式（transition、元件樣式）與上傳前的本機圖片預覽（`blob:` URL）可能需要 `style-src 'self' 'unsafe-inline'`、`img-src 'self' blob:`；`blob:` 只放在 SPA 頁面的 CSP，不影響 R-A6 的下載回應【推論：現行 SPA 的 CSP 由前端部署決定，確切值在附錄 B C1 確認】 |

### 3.3 參照自動連結與存在性

| # | 規則 |
|---|---|
| R-RF1 | **議題 key**：和 R-G1 同一個比對（`[A-Z][A-Z0-9_]{1,9}-[1-9][0-9]{0,8}`、大小寫敏感、字邊界）。**不支援 `#123`**（和 Markdown 標題、GitHub 的 `#PR` 衝突；D9 已改用專案前綴）。舊 key（`issue_keys.is_current = false`）解析到目前的單 |
| R-RF2 | **第二階段**：觀看者讀得到該單（effective 可見性，含私有單規則）→ `<a class="ox-issue" href="/issue/{目前 key}" title="{subject}">{原文}</a>`（關閉的單加 `ox-closed`，刪除線樣式）。**讀不到、不存在、key 格式對但專案不存在——一律輸出原文**，HTML 與不存在完全相同（不加 class、不加 title） |
| R-RF3 | **`@login`**（R-MN1）：被提及者在**內容所屬專案的可指派清單**裡（和編輯器自動完成用的 `GET /projects/{id}/assignees` 是同一個集合：active user、成員角色可被指派）→ `<span class="ox-mention">@{顯示名稱}</span>`（tooltip 顯示 login）；否則輸出原文。刻意**不**用「被提及者讀不讀得到這則內容」決定外觀——那會變成探測別人權限的 oracle；可指派清單本來就給能編輯議題的人看。用可指派清單而不是「所有成員」，是為了和自動完成一致：自動完成列得出來的人才會變成提及 |
| R-RF4 | **wiki 連結**：`[[標題]]`、`[[標題\|文字]]`、`[[標題#段落]]` 指本專案 wiki；`[[KEY:標題]]` 指專案 KEY 的 wiki。觀看者讀得到目標專案的 wiki → 頁存在（或命中轉址）就連過去；頁不存在而觀看者有 `edit_wiki_pages` → 紅色「建立這一頁」連結；頁不存在且不能編輯 → 顯示文字。觀看者讀不到目標專案 wiki、專案不存在、模組沒開——**一律輸出顯示文字**（沒有顯示文字就輸出 `KEY:標題`），兩者相同 |
| R-RF5 | **議題內容裡的 `[[標題]]`** 指該議題所屬專案的 wiki，規則同 R-RF4 |
| R-RF6 | **預覽**（`/markdown/preview`）用同一段程式、以呼叫者解析；`context` 指的議題或專案看不到 → `not_found`（和不存在相同）。預覽不寫入、不發通知 |
| R-RF7 | **通知摘要、匯出、搜尋摘要**用原文或純文字，不含第二階段的 title（subject）——避免把 A 單的標題帶到 B 單的讀者面前 |

### 3.4 @提及與通知

| # | 規則 |
|---|---|
| R-MN1 | 語法：`@` 前面是行首或不是 `[A-Za-z0-9_.@]` 的字元；login 是 `[A-Za-z0-9][A-Za-z0-9_.-]{0,59}`；後面不能緊接 `@` 或 `[A-Za-z0-9_-]`。code span／block、連結內不解析。比對：`principals.login` 在 0001 是**區分大小寫**唯一（`login text UNIQUE`）【事實】，所以先精確比對；沒有精確命中時再不分大小寫比對，**只命中一人才算**，命中多人輸出原文（不猜） |
| R-MN2 | **agent handle 不是提及**：`@claude-code@alice` 這種「login 後面接 `@`」整串跳過，也不會變成提及 `alice`（U19：agent 不能被 @；也避免 handle 被誤解析成人） |
| R-MN3 | **寫入時記錄**：命令引擎寫 `note_added`（留言）與 `created`／`fields_changed`（說明有變）事件的同一交易，用 R-RF3 的條件算出被提及的 user id、扣掉作者本人，寫 `issue_event_mentions`。說明改寫時只記**新增**的人（新集合 − 舊集合）。每事件最多 20 人，超過的不記（渲染照常） |
| R-MN4 | **agent 寫的提及不發通知**：`actor_kind = agent`（含內建 AI）的事件**不寫** `issue_event_mentions`；渲染照常是 chip。理由：通知是叫人注意的出口，agent 被注入後可以拿它騷擾或釣魚；`describe text_format` 明講 |
| R-MN5 | **派送**：`notify` consumer 照 05 §3.11.2，多一個原因 `mentioned`；收件人要在派送時讀得到該事件（私有單、私有留言照現行重驗），偏好 `on_mentioned = true`、專案沒有靜音。提及**不**自動關注【推論：Redmine 的 mention 只通知、不加 watcher】。讀取時第二次重驗照 05 §3.11.3 |
| R-MN6 | wiki 頁裡的提及只渲染、不通知（wiki 沒有通知，B-12） |
| R-MN7 | 匯入建立的單只在 `options.notify = true` 時寫提及（R-I11） |

### 3.5 Wiki

| # | 規則 |
|---|---|
| R-W1 | **讀取**：專案不是 archived、開了 `wiki` 模組、呼叫者持有 `view_wiki_pages`（或系統管理員）。closed 專案唯讀。沒有逐頁讀取權限（Redmine 也沒有）。看不到專案、模組沒開、沒權限——對**讀不到 wiki 的人**一律 `not_found`，和專案不存在相同；讀得到 wiki 的人才會拿到「這一頁不存在」（同為 `not_found`，畫面依 `can.create` 給「建立」按鈕） |
| R-W2 | **標題**：NFC 正規化、去頭尾空白；1–100 字元；不可含 `[ ] \| # ? / \ : < > "`、控制字元（Cc）與**格式字元（Cf：雙向控制如 U+202E、零寬字元等）**，不可以 `.` 開頭（附件檔名同樣禁 Cc、Cf）。`title_key` = 空白串換成 `_` 後做 **NFKC_Casefold**（全形與半形英數、大小寫、`ß` 與 `ss` 都視為同一標題；實作來源見附錄 B C13）；唯一性看 `title_key`（同專案未刪除的頁）。URL 用標題本身（空白換 `_`、percent-encode）；中文標題可以 |
| R-W3 | **`wiki_put`**：建立或更新一頁的內容（可同時改標題、上層）。更新要帶該頁 `lock_version` precondition；建立不帶，標題已存在 → `wiki_page_exists`（`details.page_id`——呼叫者本來就讀得到這個 wiki）。內容、標題、上層都沒變 → no-op（不產生版本、不寫事件）。每次有效更新新增一個版本。權限：`edit_wiki_pages`；受保護的頁另要 `protect_wiki_pages` |
| R-W4 | **`wiki_move`**：改標題與／或上層。改標題要 `rename_wiki_pages`；舊標題寫 `wiki_redirects`（`keep_redirect` 預設 true）；新標題已被別頁使用 → `wiki_page_exists`（新標題撞到某個轉址時刪除那個轉址）。改上層：同專案、不成環（recursive CTE）、深度 ≤ 10；`parent: null` 移到最上層。鎖序見 R-W10（先鎖 `wiki_settings`） |
| R-W5 | **`wiki_delete`（只限人）**：`delete_wiki_pages`；軟刪除；有子頁時要帶 `children: "reparent"`（子頁移到被刪頁的上層）否則 `wiki_has_children`；子頁改上層和刪除是**同一個 execution**，每個子頁在 `execution_wiki_changes` 各有一列（undo 用，R-W9），鎖序見 R-W10；指向它的轉址刪除；起始頁被刪時 `start_page_id` 設 NULL。**`wiki_restore`（只限人）**：30 天內、`manage_wiki`；標題已被占用 → `wiki_page_exists`；原上層已刪除 → 放最上層 |
| R-W6 | **`wiki_protect`（只限人）**：`protect_wiki_pages`；`{protected: bool}`；已是目標狀態 → no-op |
| R-W7 | **歷史與差異**：`view_wiki_edits`。差異逐行比對（`similar` crate），改動的行再做字詞比對；兩版各 ≤ 512 KiB；改動超過 2,000 行回前 2,000 行與 `truncated: true`。被遮蔽的版本顯示「[已遮蔽]」，差異照遮蔽後的文字算 |
| R-W8 | **回復到第 N 版**＝`wiki_put` 帶第 N 版內容，`comment` 預設「回復到第 N 版」。被遮蔽的版本不能當來源（`invalid_value`，`kind = redacted_source`） |
| R-W9 | **undo**（02 §16.7 的延伸）：`wiki_put` **更新**的 undo＝回到 `before_version` 的內容（新增一版），條件：目前 `current_version = after_version`、標題與上層沒被之後的 execution 改過、`before_version` 沒被遮蔽、人仍有編輯權限；否則 `undo_not_applicable`（`changed_since`）。`wiki_put` **建頁**的 undo＝**軟刪除該頁**（和 `wiki_delete` 相同，30 天內可還原，不硬刪），條件：之後沒有別的 execution 寫過它、沒有子頁。`wiki_move` 的 undo 同理比對標題與上層。`wiki_delete` 的 undo＝還原，並把**仍在 reparent 目標下、之後沒被別的 execution 搬過**的子頁搬回原處；其他子頁不動，undo 預覽列出。30 天（U18）。**實作現況**：`undo.rs` 的 `exec_info` 只認 `agent_direct`（owner＝委派者）與「撤銷的撤銷」（`human_direct` 帶 `reverts_execution_id`）；人自己的 wiki 寫入要能 undo 得加新路徑（M2.2 範圍，附錄 B C8）。整合寫入與匯入不走 undo（R-F10、R-I12） |
| R-W10 | **鎖序**（所有 wiki 寫入同一順序）：`operation_requests` → 專案列 `FOR SHARE` → `wiki_settings` 該專案列 `FOR UPDATE`（**建立、搬移、刪除、還原，以及帶 `title` 或 `parent` 的 `wiki_put`** 都拿：序列化樹狀結構變更，避免兩個並行搬移造成環，也讓頁數 ≤ 5000；只改內容的 `wiki_put` 與 `wiki_protect` 不拿）→ 目標頁 `FOR UPDATE` →（搬移）依 id 順序鎖新上層鏈 →（刪除帶 reparent）子頁依 id 順序 `FOR UPDATE`。唯一索引衝突（例如還原與建立撞同一標題）一律轉成 `wiki_page_exists`，不回 500。D12 遮蔽拿專案列 `FOR UPDATE`（02 §7.5），所以和任何 wiki 寫入互斥 |
| R-W11 | **事件與 outbox**：每次有效寫入寫新版本或 `wiki_events`，加一列 outbox（`kind = wiki_page`）給 wiki 索引；回執帶 `search.consistent_after`（wiki 索引的 frontier，§3.14） |
| R-W12 | **下載 `.md`**：目前版本原文；檔名 `<專案識別碼>-<標題>.md`（`filename*`）；寫 `export_events`（`wiki_page`）；權限依 A4 |
| R-W13 | **起始頁**：`wiki_settings.start_page_id`；NULL 時找 `title_key = 'wiki'` 的頁，再沒有就顯示索引 |
| R-W14 | **附件**〔A2〕：掛在頁上（不是某一版），記掛上時的版本；`wiki_put` 可同時帶 `attachments`；刪除用 `delete_attachment`（`delete_wiki_pages`，或附件作者本人且有 `edit_wiki_pages`） |
| R-W15 | **上限**：每專案 5000 頁、每頁 10,000 版（滿了 `invalid_value`，`kind = wiki_version_limit`）、每版 512 KiB |
| R-W16 | **已刪除的頁**：一般讀取路由一律 `not_found`（和不存在相同；標題可以被新頁使用）。30 天內只有 `manage_wiki` 能讀：`GET /projects/{id}/wiki/deleted`（清單）與 `…/deleted/{page_id}`（刪除時的內容；另有 `view_wiki_edits` 才含版本清單），用來決定要不要還原。agent 一律讀不到；wiki 索引在刪除時移除該頁、還原時重新加入 |

### 3.6 附件〔依建議預設 A2，待確認〕

| # | 規則 |
|---|---|
| R-A1 | **兩段式**：先 `POST /projects/{id}/uploads?filename=`（原始 body）拿 `upload_id`，再在命令文件裡引用（`attachments: [{upload_id, filename?}]`）。上傳本身不是內容寫入：不進歷史、不通知；24 小時沒用就清。上傳要求呼叫者在該專案有任一可附檔的寫入權限（`add_issues`、`edit_issues`、`add_issue_notes`、`edit_wiki_pages`）；看不到專案 → `not_found`，看得到但沒權限 → `forbidden_op`。**匿名不能上傳**（Anonymous 角色即使被授予 `add_issues` 也一樣，401） |
| R-A2 | **上限**：單檔預設 20 MB（`OXLEDGER_ATTACHMENT_MAX_BYTES`，最大 100 MB）；每份命令文件最多 10 個；每人未用的暫存 ≤ 200 MB；**每人每日上傳位元組**預設 1 GB（`OXLEDGER_UPLOAD_DAILY_BYTES`，記在 `upload_daily_usage`，組織時區換日）。超過 → 413 `payload_too_large`。**每人同時上傳**最多 3 個，超過 → 429 `rate_limited`。`OXLEDGER_FILES_DIR` 所在磁碟**剩餘空間低於門檻**（預設 5 GB，`OXLEDGER_FILES_MIN_FREE_BYTES`）→ 507 `insufficient_storage`。**收檔方式**：axum 的全域 body 上限不變，這條路由個別放寬到單檔上限；`Content-Length` 超過上限時不讀 body 直接 413；邊收邊寫暫存檔並同時算 sha256，不整份放進記憶體；收到的位元組數超過 `Content-Length` 就中止 |
| R-A3 | **內容判斷只看檔頭**（magic bytes）：PNG、JPEG、GIF、WebP 判成對應 `image/*`；**其他一律 `application/octet-stream`**，包括 SVG（U30）、HTML、PDF。不信任 `Content-Type` 與副檔名 |
| R-A4 | **去重**：`(project_id, sha256)`，只在 `state = 'present'` 的世代之間（`attachment_blobs_live_uq`）；不跨專案去重（研究 §5.5） |
| R-A5 | **讀取權限**＝讀得到容器：議題附件跟議題；留言附件跟留言（私有留言要 `view_private_notes`）；wiki 附件跟 wiki（R-W1）。看不到與不存在回同一個 `not_found` |
| R-A6 | **下載標頭**：`X-Content-Type-Options: nosniff`；`Content-Security-Policy: sandbox; default-src 'none'`；`Cache-Control: private, no-store`；`Content-Type` 用 R-A3 的結果；`Content-Disposition`：四種圖片 `inline`，其他一律 `attachment`；檔名用 `filename*` 並換掉控制字元與引號。可選 `OXLEDGER_FILES_ORIGIN`（獨立下載網域）：設了就 302 到帶短期簽章的網址（5 分鐘、綁 attachment id 與使用者，用既有 `server_secrets`）；沒設就同源回應（B-16） |
| R-A7 | **寫入**：`attach`（或 `create_issue`／`add_note`／`wiki_put` 的 `attachments` 參數）在同一交易把 upload 標 `consumed_at`、寫 `attachments`、寫事件（payload 有 attachment id 與檔名，D12 範圍）。`delete_attachment`（只限人）：議題附件要 `edit_issues`，或附件作者本人且仍有原本的新增權限；軟刪除，30 天內 undo 可還原 |
| R-A8 | **GC**：每小時：過期未用的 upload 刪列；沒有任何「未刪除、未遮蔽」附件引用、沒有未過期 upload 引用、而且最後一次刪除超過 30 天的 blob → 先標 `purged` 並提交，再刪檔（刪檔失敗下次重試） |
| R-A9 | **agent**：`get` 回附件中繼資料（`filename`、`size`、`content_type`、`created_at`）——讀者與容器相同，受眾不變；**不能**下載、上傳、刪除（MCP 沒有工具；命令文件的 `attachments` 參數對 agent 回 `forbidden_op`，`reason = attachments_people_only`）。內建 AI 同 |
| R-A10 | **掃毒**：M2 不做；留一個同步掛點（上傳完成、寫 blob 前），預設不啟用（B-17） |
| R-A12 | **遮蔽與重新上傳**：遮蔽一個附件時，同專案引用**同一個 blob 世代**的每一列附件都標 `redacted_at`／`redaction_id`（之後讀取看列上的標記，不從 blob 狀態推論），blob 清除（`purged`、`redacted`）。之後有人再上傳相同內容：**照常成功**，建立新的 blob 世代（唯一索引只約束 `present`），回應和平常完全相同——不擋、也不回「這內容曾被遮蔽」（兩者都會變成 oracle）。舊附件仍是「內容已遮蔽」，新附件是新列、新世代；要再擋就再遮蔽一次 |
| R-A11 | **儲存位置**：`OXLEDGER_FILES_DIR` 沒設定時附件功能關閉（上傳回 `forbidden_op`，`reason = attachments_disabled`；畫面不顯示附件區）。備份要和 PostgreSQL 同一時間點（研究 §5.5「備份」） |

### 3.7 Git：push 事件與 commit 連結

延伸 02 §19；R-G1–R-G9 照舊，以下是新增或改寫的規則。

| # | 規則 |
|---|---|
| R-C1 | **訂閱**：App 加訂 `push`（權限仍只讀，U45a）。`jobs.rs` 分派加 `push`；仍先確認 installation `active`、repo `accessible`、有有效連動（`FOR SHARE`，02 §19.5 同一段），否則 `ignored`，**不存 commit** |
| R-C2 | **`default_branch`**：任何帶 `repository` 物件的事件都更新 `github_repos.default_branch`；每小時全量對帳也更新 |
| R-C3 | **處理 push**：`ref` 不是 `refs/heads/*` 或 `deleted = true` → `ignored`。對 `commits[]` 的**每一個** commit：(1) **`distinct = true`**（第一次推上這個 repo）：upsert `github_commits`（`message` 截 4096 字元；`author_login` 取 `author.username`——**未驗證**，commit 的作者欄可以任意填，畫面標「GitHub 未驗證」；`sender_login` 取這次 push 的 `sender.login`（GitHub 驗證過的推送者）；**不存 email、姓名**）；upsert **不得覆寫已遮蔽的 message**（§2.5 的 `ON CONFLICT` 寫法）。每則訊息跑 R-G1，只對**目前有連動這個 repo 的專案**的 issue 建 `commit_issue_refs`（`link_id` 填鎖住的連動），而且**跳過有撤除墓碑的 (project, repo, sha)**（`project_commit_revocations`，R-C7）。(2) **`distinct = false`**（之前已推到別的分支，例如 fast-forward 合併）：`github_commits` 已有這列就不重建 ref，只更新 `last_seen`；沒有這列（先前漏收）就照 (1)。(3) **不論 distinct**：`ref` 是預設分支時 `on_default_branch = true`（只從 false 變 true），並跑修正關鍵字偵測（R-F1 (b)）——`distinct` 只用來省掉重複建 ref，不能拿來跳過預設分支的判斷。和解除連動的鎖照 02 §19.5：解除連動同交易刪 `commit_issue_refs WHERE link_id = $1`；讀取端再 join 一次有效連動 |
| R-C4 | **截斷與 GitHub 的限制**：push payload 的 `commits` 最多 2048 筆【事實：GitHub Docs push 事件說明，審查已查證，結案】。達到上限時 worker 改用 compare API（`GET /repos/{owner}/{repo}/compare/{before}...{after}`，分頁）補抓，最多 1,000 個；再多記 `result = commits_truncated`。另外 GitHub **一次推送超過 3 個 tag 或 5000 個分支時不產生 push 事件**【事實，同一頁】：這類批次推送的 commit 只能等之後的 push 或 PR 對帳帶進來；升級說明與 admin 頁（§6.10）寫明 |
| R-C5 | **force push**：被覆蓋的 commit 不刪（Redmine 也保留）；除了 `on_default_branch`，不判斷是否仍在分支上 |
| R-C6 | **顯示**：開發卡「Commits」區：sha 前 7 碼（連到 `https://github.com/{full_name}/commit/{sha}`，讀取時由 `github_repos.full_name`＋sha 組出，不用 payload 的網址）、訊息第一行（截 120 字）、作者 login（標「GitHub 未驗證」，hover 顯示推送者 `sender_login`）、時間、`on_default_branch` 時徽章「已在 〈預設分支名〉」；新到舊最多 20 筆，再多顯示「還有更多」。R-G3 照舊：讀得到 issue 才看得到它的 commit |
| R-C7 | **隱藏與撤除**：`project_commit_hidden` 依 issue 所屬專案套用（同 R-G3 的 PR 隱藏）；hide 的前提同 02 §19.7（至少一筆 ref 對到本專案中呼叫者看得到的 issue，否則 `not_found`）。`/dev/revoke` 加 `commits`：同一交易 bump 專案 epoch → 寫**撤除墓碑** `project_commit_revocations (project, repo, sha)`（持久，不隨 commit 列清除而消失）→ 刪該專案指定 sha 的 `commit_issue_refs`，以及 `issue_fix_links` 中 `pending`／`suggested`／`skipped` 的列（`applied` 的保留：它對應一次已發生的轉換，事件 payload 只有 sha7）→ 該 commit 已沒有任何 ref 時 `message` 換成 `[已遮蔽]`、設 `redacted_at` → 衍生副本照 02 §7.5 → 事件 `github_content_revoked`（不存原文）。之後同一 sha 再被推上來（別的分支、force push 回來、重送）：R-C3 看到墓碑就不在本專案建 ref，R-F3 也不建 fix link；message 不會被覆寫回原文 |
| R-C8 | **保存**：沒有 ref 的 commit 30 天後清除；repo 已無連動時同 02 §19.9 的 30 天規則。`redacted_at` 非 NULL 的列**不清除**（它和墓碑一起確保原文不會被重新寫入）；墓碑本身不清除 |
| R-C9 | **搜尋**：commit 不進索引（R-G4 的理由） |
| R-C10 | **對帳**：push 沒有每小時對帳（GitHub 沒有「列出最近 push」的權威 API【推論】）；漏掉的 push 只靠 10 分鐘補送。PR 的 commit 仍由 PR 對帳涵蓋。開發卡註明「依 GitHub 通知收集，可能不完整」 |
| R-C11 | **U45d 延伸**：commit 訊息和 PR 標題一樣，只會從系統管理員標成 `linkable` 的 repo 進來。**更正第一版**：`linkable` 只決定「能不能**新建**連動」；系統管理員關閉開放後，**既有連動照常收** PR 與 commit，要停止接收得解除連動。GitHub 設定頁的說明文字加「commit 訊息」，並寫明「關閉只影響之後的新連動」 |

**webhook 安全總表**（既有＋本文）：

| 威脅 | 處理 |
|---|---|
| 偽造 | HMAC-SHA256 常數時間比對（既有）；不符 401、不寫資料 |
| 重放（同 delivery id） | `github_deliveries` 去重 7 天（既有） |
| 重放（攔截到的合法 body 配新的 delivery id）【推論：簽章只涵蓋 body，不涵蓋 header】 | PR／check 是快照，舊快照依 R-G8 被忽略；commit upsert 冪等；**`fixed_by` 不信任 payload**：一律先用 API 確認（R-F3），而且每個（issue, PR／sha）只處理一次（唯一索引） |
| 未知或未開放的 repo | `ignored`，不存（既有＋R-C1） |
| 大量請求 | body 上限 25 MB（既有）；新增 webhook 路由並行上限 16：`ConcurrencyLimitLayer` 外面再包 `LoadShedLayer`——只有 concurrency limit 時多出來的請求會**排隊等待**（佔連線與記憶體），加 load shed 才會滿了**立刻回 503**（`tower` 已在 lockfile，只在 dev-dependencies，要移到正式依賴並開 `limit`、`load-shed` feature——不是新 crate）；GitHub 端失敗的 delivery 由 10 分鐘補送找回 |
| SSRF | 出站只有 `OXLEDGER_GITHUB_API_URL`（部署設定）；compare、PR、commit 的 API 路徑由 repo 數字 id 查出的 `full_name` 與 sha 組成，不用 payload 裡的任何 URL |
| 日誌 | 不記 payload、secret、token（T-G16 擴充到 push） |

### 3.8 `fixed_by`〔依建議預設 A3，待確認〕

「`fixed_by`」在本文的意思：議題被某個 PR 或 commit「修正」的連結，以及依修正關鍵字推進狀態（Redmine 的 commit 修正關鍵字；D9 的 `fixes AUTH-123`）。

| # | 規則 |
|---|---|
| R-F1 | **偵測來源**：(a) `pull_request` 事件 `action = closed`、`merged = true`、`base.ref` = repo 的預設分支：掃 PR 標題、PR 內文、PR 的 commit 訊息（前 250 個）；(b) `push` 到預設分支：掃這次 push **每一個** commit 的訊息（不論 `distinct`，R-C3 (3)）。**觸發來源依 A3**：建議 (i) 只認 PR 合併——(b) 偵測到的關鍵字一律只建 `suggested`（`direct_push`）提示、不自動轉換；選 (ii) 時 (b) 和 (a) 同等。PR 內文**只用來找修正關鍵字**，不存、不建一般 ref（R-G1「不看內文」照舊） |
| R-F2 | **語法**：`<關鍵字>[:]? <key>(, <key>\| and <key>)*`；關鍵字不分大小寫、整字比對；key 照 R-G1。預設關鍵字（建規則時預填）：`fix fixes fixed close closes closed resolve resolves resolved`（和 GitHub 自己的關鍵字一致【事實，附錄 A】）；可改，可加中文（例「修正」），每條規則 ≤ 10 個 |
| R-F3 | **記錄與確認**：偵測到關鍵字時，對每個比對到的 issue：專案有連動這個 repo、但**沒有符合的啟用規則** → 直接寫 `suggested`（`no_rule`），**不呼叫 API**（純提示＝U45b 原本的行為）；符合 R-F4 前兩項 → 寫 `pending`（唯一索引擋重複）。worker 用 API 確認，**每個 PR 或每次 push 只確認一次**，結果套到同一來源的所有 `pending` 列：(a) PR：`GET /repos/{owner}/{repo}/pulls/{n}` 的 `merged = true` 且 `base.ref` 是**目前**的預設分支；(b) push：只呼叫一次 `GET /repos/{owner}/{repo}/compare/{default}...{after}`（`after` 是這次 push 的 head，存在 `confirm_head`），`status` 是 `identical` 或 `behind` 就表示 `after` 和它的祖先（這次 push 的所有 commit）都已在預設分支上。確認不成立 → `skipped`（`not_on_default_branch`）；API 錯誤退避重試 5 次，之後 `skipped`（`unconfirmed`）。**列數上限**：一個 PR 或一次 push 建的 fix link（任何 state）最多 200 列，超過的不建、記 `result = fix_links_truncated`；自動轉換另受 R-F7 的 50 張限制。有撤除墓碑的 (project, repo, sha) 不建（R-C7） |
| R-F4 | **資格**（執行交易內重驗）：issue 的專案**目前**有連動這個 repo（`link_id` 有效）；有 `enabled` 規則且關鍵字、tracker 符合；issue 不是關閉狀態；不在 Triage pending；目標狀態不是目前狀態 |
| R-F5 | **執行**：命令文件 `transition`（to = 規則的目標狀態），precondition 用交易內讀到的 `lock_version`；冪等：`operation_requests.operation_id` 是 uuid，用 **UUIDv5(NS_FIXED_BY, issue_fix_links.id)** 推導（`caller_id` = 系統 principal，§2.2），不是字串 `fix:{id}`；`origin = integration`、actor＝system principal、`on_behalf_of = sponsor`、`system_operation = fixed_by`。**先以 sponsor 身分檢查可見性**：sponsor 讀不到這張單（私有單、角色可見範圍）→ `suggested`（`workflow_disallowed`——和「沒有轉換權」同一個原因碼與文案，不透露是可見性問題）。**權限與 workflow 用 sponsor 的角色**（sponsor 在該專案、該 tracker 的可用轉換）；L1 表單規則照常。事件 payload 加 `fix: {repo, pr_number \| sha7, keyword}`，**不含** PR 標題或 commit 訊息。成功 → `applied`＋`execution_id` |
| R-F6 | **不執行就改成提示**（`suggested`，原因碼）：`no_rule`（只由 R-F3 在記錄時直接寫入，不經 worker）、`direct_push`（A3 選 (i) 時）、`already_closed`、`in_triage`、`workflow_disallowed`（含 sponsor 看不到單，R-F5）、`form_rule`、`sponsor_inactive`、`sponsor_lost_permission`、`too_many`。開發卡顯示「PR #12 寫了 fixes，沒有自動轉換：〈原因〉」＋「轉換狀態」（一般的轉換對話框，用**觀看者自己**的權限＝U45b 原本的提示） |
| R-F7 | **上限**：一個 PR 或一次 push 最多自動轉換 50 張；超過的全部 `suggested`（`too_many`） |
| R-F8 | **sponsor**：PUT fix-rules 的人成為每條「內容有變或被啟用」規則的 sponsor；執行時 sponsor 要是 active user、仍有 `manage_project`，否則規則自動停用（`disabled_reason`），寫 `project_setting_events`（`fix_rule_changed`）。該表的 `operation_request_id` 是 NOT NULL，所以 worker 在同一交易先以系統 principal 建一列 `operation_requests`（`caller_id = SYSTEM_PRINCIPAL`、`operation_id = UUIDv5(NS_FIX_RULE_DISABLE, rule_id ‖ lock_version)`、`action = 'fix_rule_auto_disable'`），事件的 `actor_id` 也是系統 principal，payload 只有 `{rule_id, reason}`；同一規則同一 `lock_version` 只會停用一次（唯一鍵）。這次改成 `suggested`。sponsor 被鎖定或移出專案 → 下一次執行時停用（不另跑背景掃描） |
| R-F9 | **可見性**：fix link 是開發資訊，照 R-G3；`/projects/{id}/dev?filter=fix_skipped` 列 `suggested`、`skipped` |
| R-F10 | **和其他規則的關係**：整合寫入**不是人也不是 agent**：不重設、不累加 agent 鏈（05 §3.12.4），不計 agent 配額，不觸發 agent（M2 仍無站內觸發，U34）；通知照一般事件（摘要「由 GitHub PR #12 合併」）；undo 照 02 §16.7 的 `transition`（「轉回 X」），由有權限的人在活動頁做。**`on_behalf_of_id` 的現有使用處盤點**（integration 事件的 `on_behalf_of` 是 sponsor，但 sponsor 沒有親自動手）：`notifications.rs` 的 `acting_person()` 只認 user 與 agent → integration 回 None，sponsor 照一般關注者收通知（正確）；`agent_on_behalf` 原因與 `on_my_agents` 篩選只看 `actor_kind = agent`（不受影響）；`filter.rs` 的「最近寫過」（`Col::Wrote`，`we.on_behalf_of_id = ANY(…)`）**要排除 `origin = integration`**，否則 sponsor 會被算成寫過這些單；`undo.rs` 的 `exec_info` 只認 `agent_direct` 與撤銷的撤銷 → integration 回 None（不能走 undo 路由，符合本條）；`issue_view.rs`、`time_reports.rs`、`notifications.rs` 的顯示用 `on_behalf_of` 名稱 → integration 要顯示成「GitHub（代表 X 的規則）」而不是 X 本人；其餘出現處（`commands.rs`、`changesets.rs`、`auth.rs`、`citations.rs`、`describe.rs`、`perm.rs`、`service.rs`、`triage.rs`、`versions.rs`、`time_entries.rs`、`redaction.rs`、`forms.rs`、`db.rs`、`openapi.rs`）在 M2.6 逐一確認（附錄 B C14） |
| R-F11 | **安全說明**（設定頁顯示）：「能合併到這個 repo 預設分支的人，都能讓本專案符合規則的單轉成目標狀態。PR 內文與 commit 訊息可能出自外部貢獻者，合併前請確認裡面的修正關鍵字。」目標狀態建議用「已解決」類（非終態）而不是「已關閉」 |

### 3.9 CSV 匯出

| # | 規則 |
|---|---|
| R-E1 | **來源**：議題——一個視圖（`view_id`，用**呼叫者**的可見性執行，05 K3）或一組 Filter AST v2 條件（`where`，可帶 `project_id`）；工時——報表條件（05 §3.5.2）。沒有其他匯出入口 |
| R-E2 | **欄位**：議題預設＝視圖的欄位；`columns` 可指定（05 `views.rs` `COLUMNS` 的 key，含 `cf:<id>`）；`include_description` 加說明（原文）。讀不到的自訂欄位：視圖裡有 → 照 05 現行（`invalid_filter`／`field_unavailable`）；`columns` 指定 → 和不存在的欄位同一個 `unknown_field`。**不匯出留言**（B-25） |
| R-E3 | **值格式**：日期 `YYYY-MM-DD`；時間戳 ISO 8601 帶組織時區偏移（U42）；小數點 `.`；使用者顯示名稱；多值以 `, ` 連接；布林 `是／否`；標題列用畫面上的欄位名稱 |
| R-E4 | **公式注入**：文字型儲存格（subject、說明、string／text／list／user 型自訂欄位、名稱類）第一個字元是 `=` `+` `-` `@` `\t` `\r`，或全形 `＝` `＋` `－` `＠` 時，前面加 `'`。數字、日期型不加（值由 oxledger 格式化）。參考 OWASP CSV Injection【事實，附錄 A】 |
| R-E5 | **編碼**：UTF-8 加 BOM（Excel 才會正確開中文）、CRLF、逗號、RFC 4180 引號規則；沒有其他選項（B-26） |
| R-E6 | **上限**：10,000 列（站台設定 `export_max_rows`）；先以 `LIMIT 10001` 判斷，超過 → `export_too_large`（`details.limit`），**不回實際總數**（Q-S6），也不回半份。**逐列串流**寫出（不先組整份放記憶體）；**總位元組上限** 50 MB（`export_max_bytes`）：列數檢查已過、串流途中累計超過時，HTTP 狀態已送出無法改，伺服器**中斷連線**、`export_events.truncated = true`，前端讀到中斷就顯示 `export_too_large` 的文案、不存檔。單次 30 秒（逾時 `query_timeout`）；每人每分鐘 5 次；**每人同時只有一個匯出**（第二個 429 `rate_limited`） |
| R-E7 | **一致性**：整份在一個 `REPEATABLE READ` 唯讀交易裡讀 |
| R-E8 | **稽核**：每次成功匯出寫 `export_events`（不存內容、條件只存 HMAC，§2.7）；`GET /admin/export-events` 給系統管理員（畫面 P5） |
| R-E9 | **權限**〔A4〕：建議 (a) 讀得到就能匯出；(b) 要 `export_issues`。工時另要讀得到那些工時（05 §3.5.4）。**匿名（未登入）不能匯出**：即使 Anonymous 角色讀得到公開專案，`/exports/*` 與 wiki `export.md` 都要求已登入（401） |
| R-E10 | **agent 不能匯出**：沒有 MCP 工具；REST 對 agent token 401 |
| R-E11 | 檔名 `<專案識別碼或 all>-issues-<YYYYMMDD>.csv`；`Content-Disposition: attachment` |

### 3.10 CSV 匯入

| # | 規則 |
|---|---|
| R-I1 | **只限人**、只在單一專案內；議題要 `import_issues`（每列執行時照常需要 `add_issues` 等），工時要 `import_time_entries`（每列照常需要 `log_time`；記到別人名下要 `log_time_for_other_users`） |
| R-I2 | **上傳**：原始 body ≤ 10 MB（413）；資料列 1–5000（不含標題列）、欄 ≤ 64、單格 ≤ 64 KiB；第一列必須是標題列；空白列略過（列號照檔案算） |
| R-I3 | **編碼偵測**：有 BOM → 依 BOM（UTF-8／UTF-16LE／UTF-16BE）；否則整份是合法 UTF-8 → UTF-8；否則用 `encoding_rs` 的 Big5 解碼且沒有替換字元 → Big5，標 `encoding_guessed`（畫面請人確認）【推論：WHATWG Big5 涵蓋 Windows 繁中 Excel 存的 CP950】；都不行 → `import_invalid_file`（`kind = encoding`，`details.row`）。可在設定改編碼重新解析 |
| R-I4 | **分隔符偵測**：標題列裡（引號外）`,` `;` `\t` 出現最多者；可改 |
| R-I5 | **欄位對應**：議題——`subject`（必填）、`tracker`、`status`、`priority`、`assigned_to`、`fixed_version`、`start_date`、`due_date`、`estimated_hours`、`description`、`is_private`、自訂欄位（`cf:<id>`，只列呼叫者**寫得到**的）；工時——`spent_on`、`hours`、`activity`、`issue`（key）、`user`、`comments`。建議對應：標題列名稱與欄位顯示名稱（含別名）不分大小寫相同。未對應的欄忽略 |
| R-I6 | **值對應**：清單型（tracker、status、priority、版本、list 型自訂欄位、活動）用名稱不分大小寫相同，或 id；`value_map` 可手動指定「檔案裡的值 → oxledger 的值」。使用者用 login 或顯示名稱，顯示名稱重複 → `ambiguous_value`。議題的使用者欄位只在**本專案可指派的使用者**裡找；工時的 `user` 只在**本專案能記工時的人**（持有 `log_time` 的 active user）裡找；版本只在**本專案 open 的版本**裡找；不存在或看不到 → 同一個 `unknown_value`。日期格式 `ymd`（`YYYY-MM-DD`、`YYYY/MM/DD`）／`mdy`／`dmy`（預設 `ymd`）；小數點 `.`；布林 `是 否 yes no true false 1 0` |
| R-I7 | **前導 `'` 去除**（`options.strip_formula_quote`，預設 true）：儲存格是 `'` 接 R-E4 觸發字元時去掉 `'`，和匯出對稱。oxledger 從不執行儲存格內容 |
| R-I8 | **試跑**：每列組成命令文件，用 `commands::check_as`（05 `validate` 同一段：可見性 → precondition → 權限 → workflow → L1 → 欄位）以**匯入者身分**檢查，不寫入。結果寫 `import_rows`（`ok`／`error`、錯誤碼、欄位 key，**不存儲存格內容**），state → `validated`。試跑不是保證 |
| R-I9 | **每列的命令文件**：議題列＝`create_issue`（＋ `status` 不是初始狀態時加 `transition`；＋專案開了 Triage、`options.skip_triage = true` 且匯入者有 `triage_issues` 時加 `triage_decide accept`）；工時列＝`log_time`。冪等：`operation_requests.operation_id` 是 uuid，用 **UUIDv5(NS_IMPORT, import_id ‖ row_no)** 推導（`caller_id` = 匯入者），不是字串 `import:{id}:{row}`；`command_executions.import_id` 記這次匯入；說明的 `format` 是 `markdown` |
| R-I10 | **執行**：`run` 檢查 state `validated` 且 `lock_version` 和試跑時相同；背景 worker（`FOR UPDATE SKIP LOCKED` 取 `imports`，依列號順序）**每列一個交易**；列失敗記錯誤、繼續下一列（不中斷整批，B-28）；`cancel` → 做完目前這列停，剩下 `skipped`。程序中斷後從第一個 `pending` 列繼續（冪等鍵保證不重複） |
| R-I11 | **通知**：`options.notify`（預設 false）。false 時這次匯入的 execution 不產生任何通知原因、不寫提及、不自動關注 |
| R-I12 | **「撤銷匯入」**（`close-created`，帶目標關閉狀態）：這次匯入建立的每張單，若之後沒有任何非本匯入的 execution 改過它（`issue_field_origins` 全部仍指向本匯入、沒有新留言）且匯入者有轉到該狀態的權限 → `transition`（02 §16.7「關閉為誤建」，create_issue 不刪除）；其餘列成「無法撤銷」清單附原因。工時列用 `delete_time_entry`。30 天內可用（U18） |
| R-I13 | **暫存與保存**：`imports.file` 在完成（`done`／`canceled`／`failed`）24 小時後、`DELETE /imports/{id}` 時、或專案發生 D12 遮蔽時設 NULL；`import_rows` 保留 30 天；沒動作的匯入 7 天後 `expired` |
| R-I14 | **存在性**：別人的匯入、不存在的匯入 → 同一個 `not_found`；列錯誤只有錯誤碼與欄位 key，不帶值 |
| R-I15 | **搜尋**：每列照一般寫入產生 outbox；完成畫面提示「新單可能要幾秒後才搜尋得到」（03 Q-S10 的精神） |

### 3.11 命令文件的新 op（補 02 §6.1、05 §3.10）

```jsonc
// wiki_put：建立或更新一頁；每份文件只能有這一個 op（同 create_version 的規則）
{ "op_id": "…", "kind": "wiki_put",
  "target": {"project": "AUTH", "page": "部署流程"},   // page：標題或 page_id；建立時是新標題
  "body": "…markdown…", "comment": "補上回滾步驟",
  "title": "部署與回滾",                               // 可選：同時改名（要 rename_wiki_pages）
  "parent": {"page": "維運"},                          // 可選；null＝最上層
  "attachments": [{"upload_id": "…"}] }                // 只限人〔A2〕
// preconditions：更新 [{"wiki_page": "<page_id>", "lock_version": 7}]；建立不帶（帶了 → invalid_argument）

{ "kind": "wiki_move",    "target": {…}, "title": "…", "parent": {…} | null, "keep_redirect": true }
{ "kind": "wiki_delete",  "target": {…}, "children": "reparent" }              // 只限人
{ "kind": "wiki_restore", "target": {"project": "AUTH", "page_id": "…"} }       // 只限人
{ "kind": "wiki_protect", "target": {…}, "protected": true }                    // 只限人

// 附件〔A2〕：只限人
{ "kind": "attach", "target": {"issue": "AUTH-12"} | {"note": "<note_id>"} | {"project": "AUTH", "page": "…"},
  "attachments": [{"upload_id": "…", "filename": "截圖.png"}] }
{ "kind": "delete_attachment", "target": {"attachment": "<id>"} }

// 既有 op 的新參數
// create_issue、update_fields（description）："format": "markdown"   create_issue 省略＝markdown；
//   update_fields 省略＝保留原格式（R-M9）；agent 只能 markdown
// add_note："format": "markdown", "attachments": [...]
```

- **驗證順序**（02 §6.7 的延伸）：wiki op 的目標解析用 `parse_for`：專案看不到、模組沒開、沒有 `view_wiki_pages` → `not_found`；更新類的頁不存在 → `not_found`；之後才檢查 precondition、權限、保護、上限。
- **agent 能用的**：`wiki_put`、`wiki_move`（受 §3.12）；`format` 只能 `markdown`；`attachments` → `forbidden_op`（`attachments_people_only`）。其他新 op 對 agent → `invalid_request`（同 `delete_time_entry` 的現行做法）。
- **D20 政策**：`delegation_command_policies` 新列 `wiki_put`、`wiki_move`，新委派的預設 mode 依 A5（建議 (c)：`direct`＋活動頁與首頁提示）；委派開了 `read_own_time` 時另依 A6（建議一律送審，R-AG11）；wiki 沒有欄位政策。
- **審閱路徑**：review 模式的 wiki op 變成 ChangeSet；提案內容存整份 body；審閱畫面顯示和目前版本的差異（R-W7 同一個函式）；套用時頁已被改 → `conflict`（照 02 的 revise／重提流程）。

### 3.12 agent 與內建 AI 的規則

| # | 規則 |
|---|---|
| R-AG1 | **讀 wiki 的範圍**：專案 P 在委派內（02 §3.5）、`wiki` 模組開、而且 **W(P) ⊇ A(P)**（讀得到 wiki 的人包含所有讀得到 P 公開議題的人）。**W(P) 用新函式 `wiki_audience()` 算，不能沿用 `holders()`**：`holders()` 一律 join `enabled_modules … module = 'issue_tracking'`，議題模組關、wiki 開的專案會算出空集合；而且以 `Vis::Public` 只算 `issues_visibility` 是 all／default 的角色，`own` 角色若持有 `view_wiki_pages` 會被漏掉——W(P) 被低估，R-AG3 的 W(P) ⊆ ∩A(R) 就可能誤判通過（實際讀者比算出來的多＝外洩）。`wiki_audience(P, perm)`：專案開了 `wiki` 模組且未封存時，持有 `perm` 的使用者，**不論角色的 `issues_visibility`**（相當於 `Vis::Any`）；Non member、Anonymous、系統管理員照 `holders()` 現行的處理方式。W(P)＝`wiki_audience(P, view_wiki_pages)`，H(P)＝`wiki_audience(P, view_wiki_edits)`。議題模組關閉時 A(P) 是空集合：agent 讀得到 wiki（W ⊇ ∅），但目的地讀者非空的寫入一律過不了 M9。不成立時，對 P 的任何 wiki 工具回 `forbidden_op`（`reason = wiki_not_delegable`），**不分頁存不存在**；委派頁顯示「這個專案的 wiki 讀者比議題讀者少，agent 不能讀寫 wiki」。**舊版本**的讀者是 H(P)＝`view_wiki_edits` 持有者（⊆ W(P)），所以 agent 讀舊版（`wiki_get` 帶 `version`）另要 **H(P) ⊇ A(P)**，否則 `forbidden_op`（`reason = wiki_history_not_delegable`）；0032 把 `view_wiki_edits` 授給和 `view_wiki_pages` 相同的角色，預設情況兩者相等 |
| R-AG2 | **理由**：02 §3.8 的保證是「agent 寫到的地方，讀者不比它讀過的任何來源多」，以 A(R) 為下界。W(P) ⊇ A(P) 時，從 wiki 讀到的內容讀者 ⊇ A(P)，原證明不變，**不需要新例外、也不需要 taint**；wiki 比議題窄的專案就和工時同類，本文不開例外 |
| R-AG3 | **寫 wiki 的目的地檢查**：wiki 內容的讀者是 W(P)；M9 要求 W(P) ⊆ ∩A(R)。和 R-AG1 合起來，agent 只寫得進「W(P) = A(P) 且所有來源受眾都包含它」的 wiki；失敗回 `audience_expansion_forbidden`（02） |
| R-AG4 | **來源記錄**：讀 wiki 把 P 加進工作脈絡的來源集合 R（和讀議題相同）；wiki 提案的來源快照照 02 §3.8 |
| R-AG5 | **受保護頁**：agent 一律不能寫（`forbidden_op`，`reason = wiki_protected`），即使委派者有 `protect_wiki_pages` |
| R-AG6 | **配額與熔斷**：wiki 寫入計入 `agent_write_log`（`wiki_write`）與既有配額；U17「同一張 issue 10 分鐘 > 10 次」延伸為「同一 wiki 頁 10 分鐘 > 10 次」→ 暫停 principal（同既有原因碼） |
| R-AG7 | **鏈上限**：05 §3.12.4 的鏈是每張 issue 的；wiki 沒有鏈，連續寫入由 R-AG6 擋【推論：M2 沒有站內觸發，agent 互改同一頁的迴圈只剩外部 runtime 自己造成，熔斷足夠】 |
| R-AG8 | **內建 AI**：工具集合加 `wiki_list`、`wiki_get`；`wiki_put`／`wiki_move` 經 `command_submit`，照委派政策。**time_taint 評估**：wiki 讀取在 R-AG1 下不比 A(P) 窄，不需要新的 taint；工時汙染（05 Q3）照舊——讀過 `time_list` 的 run 寫 wiki 也一律送審 |
| R-AG9 | **其他新物件**：附件內容、匯入、匯出、fix rules、commit 隱藏與撤除、wiki 刪除／還原／保護、wiki 設定——agent 都沒有。commit 與 fix link 經 `get include: ["dev"]` 讀，受眾同議題（02 §19.6） |
| R-AG10 | **Markdown**：agent 寫的連結套 R-M3 的嚴格規則；agent 寫的提及不發通知（R-MN4） |
| R-AG11 | **工時例外延伸到 wiki**〔依建議預設 A6，待確認〕：外部 agent 的委派開了 `read_own_time`（05 A1(c)）時，該委派的 `wiki_put`、`wiki_move` 不論政策**一律送審**（`review:time_read_delegated`）。理由：server 不記外部 agent 的讀取紀錄（02 §3.8 M2），無法只對「讀過工時之後」的寫入送審；wiki 是全專案的長期文件，又沒有通知（B-12）。內建 AI 照 time_taint（只在該 run 讀過 `time_list` 後送審），不受這條影響 |

### 3.13 D12 遮蔽涵蓋

照 02 §7.5 的交易順序：專案列 `FOR UPDATE`、`security_epoch + 1` → 目標列 `FOR UPDATE` → 管理者檢查 → 換成 `[已遮蔽]`、寫遮蔽紀錄（不存原文）→ `derived()` → outbox。`redaction.rs` 的 `Target` 新增：

| 目標 | 誰能遮蔽 | 換掉什麼 | 備註 |
|---|---|---|---|
| `WikiVersion {page, version}` | `manage_project` 或系統管理員 | 該版 `body` 與 `comment`（修改說明也可能含機密） | wiki 索引重建該頁；該版不能再當回復來源 |
| `WikiTitle {page, new_title}` | 同上 | 頁的**所有**歷史標題：`wiki_events` 的 `renamed` payload、`execution_wiki_changes` 的標題欄、`wiki_redirects`（刪除）；同交易把頁改成 `new_title` | 必須給新標題（標題不能是「[已遮蔽]」，會撞名） |
| `WikiEvent {page, event}` | 同上 | 該事件 payload | — |
| `Attachment {id}`〔A2〕 | 同上 | `filename` → `[已遮蔽]`；**blob 世代立即清除**（`purge_reason = redacted`），同專案引用同一 blob 世代的**每一列**附件都標 `redacted_at`／`redaction_id`，一起變「內容已遮蔽」（同內容就是同一份機密，R-A12）；事件 payload 的檔名 | 確認框寫「本專案其他相同內容的附件（可能包含你看不到的）也會一起無法下載」，**不給數量**（避免 oracle）；之後重新上傳同內容照常成功（R-A12） |
| 留言（既有 `Note`，擴充） | 既有 | 同交易把**掛在這則留言的附件**逐一做 `Attachment` 遮蔽（清本體） | 留言被遮蔽、附件還能下載不合理；確認框列出附件檔名 |
| 說明（既有的說明變更遮蔽，擴充） | 既有 | 遮蔽對話框列出**議題層附件**（`note_id IS NULL`），讓管理者勾選是否一併遮蔽，**預設不勾**；勾了的逐一做 `Attachment` 遮蔽 | 附件不一定和說明的機密有關（B-45） |
| commit（擴充既有 `/dev/revoke`） | 同 02 §19.9 | R-C7（撤除墓碑＋`redacted_at`） | 同 02 §19.9 |

`derived()` 擴充（同交易、依專案）：`execution_wiki_changes` 的標題欄換成標記；該專案所有 `imports.file` 設 NULL（`import_rows` 不含內容，不動）；被遮蔽的留言或事件的 `issue_event_mentions` 刪除；既有的提案內容、群組標籤、undo 前後值、AI 訊息照舊。Markdown 渲染快取不用動（R-M6）。

匯出檔不在伺服器上、收不回來（同 D12「已送出的 webhook 收不回來」）——遮蔽確認框加一句「已經匯出的 CSV 不受影響」。

### 3.14 搜尋

- **wiki 用獨立的 tantivy 索引**（同一套 tokenizer、D15 查詢端規則）。outbox consumer **沿用 0018 的世代機制**：命名 `wiki@<世代 uuid>`（對照 `search@<世代 uuid>`）、同一組 `state`（bootstrapping／active／retained／retired）、同一套藍綠重建與租約；它和 `notify` 一樣**消費所有 outbox 列、對非 wiki 的列只推進 cursor**，所以會一路推進到 frontier，不會卡住 outbox GC（0018 的 GC 保留到 active、bootstrapping、retained 消費者中最小的 cursor；`/metrics` 加 `wiki_outbox_pending`）。每頁一份文件＝目前版本：標題（權重 3.0）、內文純文字（1.0）、`project_id`。不動 issue 索引的 `Layout`。
- **可見性**：查詢時先算「呼叫者讀得到 wiki 的專案集合」（R-W1）當 filter，top-k 回 DB 複驗（研究 §5.5）；agent 再交集 R-AG1 的專案。
- **API**：新端點 `GET /wiki/search?q=&project=&cursor=`（只回 wiki 結果：`{project, title, snippet, updated_at}`；議題搜尋 `GET /issues/search` 不變）。兩種結果不混排（兩個索引的分數不可比）。MCP `search` 收 `kinds`，結果分 `issue`、`wiki` 兩組（§4）。
- **一致性**：wiki 寫入回執的 `consistent_after` 用 wiki 索引的 frontier；落後時 `search_lagging`（03 Q-S4）。
- **議題索引**：說明與公開留言改索引 Markdown 抽出的純文字（R-M11）；layout 版本 +1，升級後藍綠重建一次。E1 nDCG@10 不得低於 0.810（05 M1.7 的門檻）。

---

## 4. MCP 工具

### 4.1 新增與修改一覽

| 工具 | 新／改 | 內容 | 誰 |
|---|---|---|---|
| `wiki_list` | 新 | `{project, under?: 頁, cursor?}` → `[{page_id, title, parent_id, updated_at, version}]`、`truncated`；只列 R-AG1 成立的專案 | 外部 agent、內建 AI |
| `wiki_get` | 新 | `{project, page, version?}` → `{page_id, title, body, format: "markdown", version, lock_version, protected, parent, children[], attachments[] (中繼資料), updated_at, updated_by}`；`version` 要委派者有 `view_wiki_edits` 且 H(P) ⊇ A(P)（R-AG1）；轉址時回 `redirected_from` | 同上 |
| `command_submit`、`changeset_propose`／`revise` | 改 | 收 `wiki_put`、`wiki_move`；既有 op 的 `format`（只能 `markdown`） | 同上 |
| `search` | 改 | 收 `kinds: ["issue", "wiki"]`（預設 `["issue"]`）；結果分 `issue`、`wiki` 兩組 | 同上 |
| `get` | 改 | 回 `description_format`；`include: ["dev"]` 的 `dev` 加 `commits[]`（sha7、第一行、`author_login`〔標 `author_verified: false`〕、`sender_login`、時間、`on_default_branch`）與 `fix_links[]`（`pr_number`／`sha7`、keyword、state、reason）；`attachments[]`（中繼資料） | 同上 |
| `describe` | 改 | 新主題 `text_format`：方言（R-M1）、議題 key 與 `[[頁面]]` 語法、**「你寫的 @提及不會通知任何人」**、外部連結與圖片規則；`describe agent_context` 列出每個委派專案的 `wiki: readable｜writable｜unavailable(reason)` | 同上 |

### 4.2 工具描述草稿（英文，進 `mcp.rs`）

- `wiki_list`: "List wiki pages of a delegated project you can read. Projects whose wiki has fewer readers than their issues are not available to agents (forbidden_op/wiki_not_delegable). Results are a tree; use wiki_get for content."
- `wiki_get`: "Read one wiki page (current version, or an older version if allowed). Body is CommonMark; issue keys like AUTH-123 and [[Page]] links are plain text in the source. Write with command_submit wiki_put, passing the page's lock_version as the precondition."
- `command_submit`（補充句）: "wiki_put creates or updates one page per document. You cannot edit protected pages, attach files, delete or protect pages. @mentions you write render but never notify anyone."

### 4.3 direct 還是 review（D20、02 §16.3）

| op | 新委派預設 | 依據 |
|---|---|---|
| `wiki_put`、`wiki_move` | **direct**，另在活動頁與首頁提示 agent 最近改的 wiki〔A5 建議 (c)，待確認〕；委派開了 `read_own_time` 時一律送審〔A6，R-AG11〕 | D20「新委派預設全部直接寫入」；每版都留歷史、可 undo（R-W9）；寫入範圍已被 R-AG1／R-AG3 限在 W = A 的專案；wiki 沒有通知，所以補一個看得到的提示 |
| 既有 op 帶 `format` | 不變 | `format` 不是欄位政策 |

既有委派**不**自動加 wiki op（02 §16.3 規則 1）；委派頁以「新操作」標記提示，委派者自己加。

### 4.4 角色範本與內建 AI 工具集合

- 角色範本（05 §4.4、U33 完整允許清單）：wiki op **不進任何既有範本的預設**；新增範本「文件維護」＝ `wiki_put`、`wiki_move`＋讀取工具，預設 review。
- 內建 AI（05 §4.5）：加 `wiki_list`、`wiki_get`；`command_submit` 的 wiki op 照使用者對 `oxledger-ai@<login>` 的委派政策；讀過 `time_list` 的 run 一律送審（05 Q3）不變。

---

## 5. 錯誤碼

沿用 02 §4.3 格式與 02、05 既有的碼；新增：

| HTTP | code | 何時 | retryable | next_actions |
|---:|---|---|---|---|
| 409 | `wiki_page_exists` | 建立或改名時標題已被同專案未刪除的頁使用；`details.page_id` | 否 | `fix_input`（換標題，或改用更新） |
| 409 | `wiki_has_children` | 刪除有子頁的頁、沒帶 `children: "reparent"` | 否 | （SPA） |
| 422 | `wiki_title_invalid` | R-W2 不通過；`details.reason`：`length`、`char`（`details.char`）、`leading_dot` | 否 | `fix_input` |
| 413 | `payload_too_large` | 上傳超過 R-A2、匯入檔 > 10 MB、wiki 內容 > 512 KiB；`details.limit` | 否 | `fix_input` |
| 422 | `import_invalid_file` | `details.kind`：`encoding`（`row`）、`too_many_rows`、`too_many_columns`、`cell_too_large`（`row`、`column`）、`no_header`、`empty`、`parse`（`row`） | 否 | （SPA） |
| 409 | `import_state_invalid` | 動作和匯入狀態不符（例：沒試跑就 run、執行中改設定）；`details.state` | 否 | （SPA） |
| 409 | `import_settings_changed` | run 時 `lock_version` 和試跑時不同 | 否 | （SPA）重新試跑 |
| 422 | `import_mapping_invalid` | 對應不合法：缺 `subject`、同一欄位對應兩次、對應到讀不到或寫不到的欄位（和不存在同形狀）；`details.field` | 否 | （SPA） |
| 422 | `export_too_large` | 超過 R-E6 上限；`details.limit`（不給實際數量） | 否 | （SPA）「縮小範圍」 |
| 507 | `insufficient_storage` | 附件儲存目錄所在磁碟剩餘空間低於門檻（R-A2） | 是 | （SPA）稍後再試 |
| 422 | `fix_rule_invalid` | 規則不合法：關鍵字空白／重複／超過 10 個、目標狀態不存在、tracker 不在專案、超過 5 條；`details.reason`、`details.index` | 否 | （SPA） |

**列錯誤碼**（`import_rows.error_code`，不是 HTTP）：沿用命令引擎的碼（`forbidden_op`、`required_field_missing`、`invalid_value`、`workflow_disallowed`、`form_rules_changed`…），另加 `unknown_value`、`ambiguous_value`、`bad_date`、`bad_number`、`bad_boolean`。

沿用但新增 `details` 的：

| code | 新的 `details` |
|---|---|
| `forbidden_op` | `reason`：`wiki_not_delegable`、`wiki_history_not_delegable`、`wiki_protected`、`attachments_people_only`、`attachments_disabled`、`agent_format`（agent 寫 `plain`，或更新 `plain` 說明時省略 `format`） |
| `rate_limited`（既有） | 每人同時上傳超過 3 個（R-A2）、同時第二個匯出（R-E6） |
| `invalid_value` | `kind`：`redacted_source`、`wiki_version_limit`、`wiki_depth`、`wiki_cycle`、`upload_unknown`（不存在、過期、別人的、別專案的——同一形狀）、`attachment_limit` |
| `undo_not_applicable` | `reasons` 加 `changed_since`（wiki）、`redacted_source` |
| `audience_expansion_forbidden`（02） | `details.destination = "wiki"` |
| `not_found` | 不變；本文所有新路由與新 op 都用它表達「看不到或不存在」 |

02 §14.2 的對照加：`wiki_page_exists` → `fix_input`；`forbidden_op`（`wiki_not_delegable`、`wiki_protected`、`attachments_people_only`）→ `ask_human`、`stop`——**不**改寫到別的專案或別的頁繞過。

---

## 6. 畫面規格

共通規則沿用 05 §6（版面 B、四種共通狀態元件、寫入按鈕與冪等、`conflict` 不覆蓋、agent 顯示〔agent〕、文案）。本節只寫新畫面與既有畫面的修改。

### 6.1 Markdown 顯示元件 `<RichText>`

- **輸入**：API 的 `*_html` 字串＋`format`。**只有這個元件可以用 `{@html}`**，而且只接 `*_html` 欄位（lint 規則，R-M10）。
- **樣式**：標題 h1–h6 縮成內文層級（議題頁說明的 h1 ＝ 18px，避免比頁標題大）；表格可水平捲動（外包 `overflow-x: auto`）；code block 等寬字、可水平捲動、右上「複製」按鈕；工作清單 checkbox 顯示但停用；`ox-closed` 的議題連結加刪除線；`ox-mention` 是圓角 chip；`ox-ext` 連結後面小字顯示主機（`data-ox-host`），新分頁開。
- **agent 內容**（元素上有 `data-ox-agent`）：外部連結點擊先開確認框「即將前往 〈主機〉，網址：〈完整網址〉」＋「前往」「取消」（同 05 §6.19）。
- **外部圖片**（`class="ox-img-ext"` 的連結）：顯示成圖示＋「外部圖片：說明（主機）」，不載入。
- **本站圖片**：最大寬度 100%、點擊開燈箱（原圖、Esc 關閉、左右鍵切換同容器圖片）；載入失敗顯示「圖片無法顯示」。
- **`plain`**：同一元件，HTML 已是純文字段落；不顯示任何格式提示。
- **行動版**：表格與 code block 水平捲動，不縮字；燈箱全螢幕。
- **無障礙**：議題連結的 `title`（subject）同時放 `aria-label`；mention chip `aria-label="提及 〈名稱〉"`。

### 6.2 Markdown 編輯器 `<MarkdownEditor>`（說明、留言、wiki）

- **版面**：分頁「編輯」「預覽」（寬度 ≥ 1280 時改左右並排，可切換）；工具列：粗體、斜體、標題、清單、編號清單、工作清單、引用、程式碼、連結、表格、附檔（A2）、「說明」（開語法小抄側欄）。
- **自動完成**：輸入 `@` 後 2 字開始，下拉列出**本專案可指派清單**（既有 `GET /projects/{id}/assignees`，前端本機篩選）；這和 R-RF3 判斷 chip 與通知的集合相同——自動完成列得出來的人才會變成提及；輸入議題 key 前綴與 `-` 後列出最近看過的議題（本機記錄，不打 API）；輸入 `[[` 列出本專案 wiki 頁（讀得到 wiki 時）。
- **預覽**：切到預覽或並排模式下停止輸入 500 ms 後 `POST /markdown/preview`（帶 `context`）；預覽區用 `<RichText>`；預覽失敗顯示「無法預覽，內容沒有遺失」。
- **附檔**（A2）：拖放、貼上剪貼簿圖片、按鈕選檔 → 每個檔案一列進度條（`POST /uploads`）；完成後在游標處插入 `![檔名](attachment:檔名)`（圖片）或 `[檔名](attachment:檔名)`；送出時把 upload id 放進命令文件的 `attachments`。超過大小 → 該列紅字「檔案超過 20 MB」；超過每日配額 → 「今天的上傳量已達上限」；`insufficient_storage` →「伺服器儲存空間不足，請通知管理者」；同時超過 3 個時其餘排隊（前端自己排，不打 API）；`attachments_disabled` 或未登入 → 不顯示附檔按鈕。
- **舊內容轉換**（R-M9）：編輯 `plain` 說明時，編輯器上方黃色提示「這段說明原本是純文字。儲存後會以 Markdown 顯示，請先看預覽。」
- **agent 內容**（R-M3）：要編輯的原內容是 agent 內容（API 的 `description_by_agent`、wiki 頁的 `updated_by.kind = agent`）時，上方提示「這段內容含 agent 寫入的連結。儲存後會以你的名義顯示，連結不再有額外的確認保護，請先檢查連結。」
- **字數**：右下 `已用 / 上限`（留言 64 KiB、wiki 512 KiB，以位元組計，顯示成「約 N 字」）；超過時送出按鈕停用。
- **未儲存離開**：有變更時離開頁面跳確認（`beforeunload`＋SPA 路由守衛）；草稿存 `localStorage`（鍵含使用者 id 與目標），回來時提示「有未送出的草稿」＋「還原」「捨棄」。
- **行動版**：工具列收成一列可水平捲動；預覽只用分頁模式。

### 6.3 Wiki 頁 `/projects/{key}/wiki/{title}`

- **入口**：專案分頁「Wiki」（新內建分頁，§2.3）；左欄專案下方捷徑（U24：PM、設計師預設有）。
- **版面**（中間工作區）：
  1. 麵包屑：`Wiki › 維運 › 部署與回滾`；右側動作列：「編輯」「⋯」（改名／移動、保護、歷史、下載 .md、刪除）。沒權限的動作照 05 §6 共通規則：按鈕停用＋hover 說明，不隱藏；`can.history = false` 時「歷史」不出現（那是讀取權限，不是動作）。
  2. 標題（h1）＋狀態列：「第 12 版・Carol Hsu 於 11/10 14:20 更新」；agent 更新時「claude-code@alice〔agent〕於 …」；受保護時鎖頭圖示＋「受保護」。
  3. 內文 `<RichText>`。
  4. 附件區（A2）：檔名、大小、上傳者、時間；圖片顯示縮圖格（用原圖縮放，不另產縮圖）。
  5. 子頁清單：「子頁面」＋連結。
- **左側頁面樹**（寬度 ≥ 1024 時在中間工作區左側 240px，可收合；行動版收成頂部「頁面」按鈕開抽屜）：樹狀、可展開、目前頁高亮、頂部搜尋框（篩樹，不打 API）、底部「新增頁面」。
- **轉址**：`redirected_from` 有值時頂部灰條「由〈舊標題〉轉址過來」。
- **狀態**：
  - 頁不存在、讀得到 wiki、`can.create`：「這一頁還不存在。」＋「建立〈標題〉」（進編輯器，標題預填）。不能建立：「這一頁還不存在。」。
  - 讀不到 wiki：照 05 §6「找不到這個頁面，或你沒有權限查看。」（和不存在相同）。
  - wiki 是空的：起始頁位置顯示「這個專案還沒有 wiki 頁面。」＋「建立第一頁」（預設標題「Wiki」）。
  - closed 專案：所有寫入按鈕停用，頂部提示「專案已關閉，wiki 唯讀」。
- **鍵盤**：`E` 編輯、`H` 歷史（有權限時）。

### 6.4 Wiki 編輯 `/projects/{key}/wiki/{title}/edit`

- 標題欄（建立時可編；既有頁要改名走「改名／移動」對話框）、上層選擇（下拉樹，建立時）、`<MarkdownEditor>`、「修改說明」（單行，≤ 255）、「儲存」「取消」。
- 送出 `POST /commands` `wiki_put`（帶 `lock_version`）。
- **conflict**：不覆蓋；頂部紅條「這一頁在你編輯時被 〈名稱〉 更新了（第 13 版）。」＋「比較差異」（開對話框，左：最新版、右：你的內容，用 diff API 的同一個呈現）＋「以我的內容為基礎重新儲存」（重新抓 `lock_version` 後再送一次，需二次確認）＋「放棄我的修改」。輸入保留。
- `wiki_page_exists`：標題欄下紅字「已經有同名的頁面」＋連到那頁。
- `wiki_title_invalid`：標題欄下紅字說明哪個字元不能用。
- 受保護頁且沒有 `protect_wiki_pages`：「編輯」按鈕停用，hover「這一頁受保護，只有有保護權限的人能編輯」。

### 6.5 改名／移動、刪除、保護、還原

- **改名／移動對話框**：新標題、上層（樹選擇，排除自己與子孫）、「保留舊標題的轉址」（預設勾）；送 `wiki_move`。
- **刪除對話框**：「刪除〈標題〉？30 天內可以由 wiki 管理者還原。」有子頁時加「它的 N 個子頁會移到〈上層〉下面」（N 是呼叫者看得到的子頁數——wiki 沒有逐頁權限，等於全部）；送 `wiki_delete`（`children: "reparent"`）。
- **保護**：「⋯ › 保護此頁／取消保護」，直接送 `wiki_protect`，toast「已保護」。
- **已刪除頁**（`manage_wiki`）：wiki 索引頁底部「最近刪除（30 天內）」清單＋「檢視」（R-W16，唯讀、頂部灰條「這一頁已刪除」）＋「還原」。

### 6.6 Wiki 歷史與差異

- **歷史** `/…/history`：表格：版本、時間、作者（agent 加〔agent〕；核准者顯示「Alice 核准」）、修改說明、大小變化（+120／−40 字元）、兩個 radio（比較起點、終點）＋「比較選取的版本」。被遮蔽的版本：修改說明照顯示、內容欄標「[已遮蔽]」，radio 仍可選。每列「檢視」「回復到這一版」（`edit_wiki_pages`；被遮蔽的停用，hover「這一版已遮蔽」）。
- **單版檢視**：同頁版面，頂部黃條「你在看第 8 版（目前是第 12 版）」＋「回復到這一版」「看與目前的差異」。
- **差異**：兩欄（寬度 ≥ 1024）或統一格式（窄螢幕自動切換）；刪除行紅底、新增行綠底，行內字詞差異加深；未改動的段落折疊「⋯ 展開 24 行」；`truncated` 時底部「差異太大，只顯示前 2,000 行」。顏色以外另用 `−`／`+` 前綴（不只靠顏色）。
- **D12**（`manage_project`）：歷史每列「⋯ › 遮蔽這一版」→ 05／02 的遮蔽對話框（原因碼＋說明）；頁的「⋯ › 遮蔽標題」要求輸入新標題。

### 6.7 Wiki 索引與搜尋

- **索引** `/projects/{key}/wiki/index`：全樹展開清單＋依更新時間排序的切換；每列標題、更新時間、更新者。
- **全站搜尋頁**：結果區上方分頁「議題」「Wiki」（Wiki 分頁打 `GET /wiki/search`）；Wiki 結果卡：專案徽章、頁標題、摘要（命中字詞加粗）、更新時間。只在使用者至少讀得到一個專案的 wiki 時顯示「Wiki」分頁。剛寫入的頁搜不到時沿用 03 Q-S10 的「同步中」提示。
- **agent 最近改的 wiki**〔A5 建議 (c)〕：專案活動頁加篩選「agent 改的 wiki」；首頁「最近」區塊加一張卡「agent 最近 7 天改了這些 wiki 頁」：頁名、agent（〔agent〕標記）、委派者、時間、「看差異」。只顯示呼叫者讀得到的 wiki；沒有就不顯示卡片。A5 選 (a)／(b) 時不做。

### 6.8 議題頁的修改

- 說明與留言改用 `<RichText>`／`<MarkdownEditor>`。
- **附件卡**（A2）：說明下方；檔名（圖示依類型）、大小、上傳者、時間、「下載」；圖片顯示縮圖格；作者或有權限者 hover 出現「刪除」（確認框「刪除附件？30 天內可以復原。」）。留言的附件顯示在該留言底部。
- **開發卡**（02 §19 F30 的延伸）：
  - 既有 PR 區不變。
  - 新「Commits」區（R-C6），區塊標題右側灰字「依 GitHub 通知收集」；作者欄顯示「alice（GitHub 未驗證）」，hover 顯示「由 bob 推送」（`sender_login`）。
  - **修正連結**：PR 或 commit 有 fix link 時加徽章：`applied` →「已自動轉為〈狀態〉」（hover：規則、sponsor、時間）；`suggested`／`skipped` → 黃色「寫了 fixes，沒有自動轉換：〈原因文案〉」＋「轉換狀態」按鈕（開一般轉換對話框，預選規則的目標狀態）。
  - 原因文案：`no_rule`「本專案沒有啟用對應的規則」、`already_closed`「單已經關閉」、`in_triage`「單還在分流」、`workflow_disallowed`「規則設定者無法對這張單做這個狀態轉換」（含設定者看不到這張單，文案刻意相同）、`direct_push`「直接推到預設分支，依設定只提示」、`form_rule`「目標狀態有必填欄位沒填」、`sponsor_inactive`／`sponsor_lost_permission`「規則設定者已失去權限，規則已停用」、`too_many`「一次修正的單太多（超過 50 張）」、`not_on_default_branch`「沒有合併進預設分支」、`unconfirmed`「無法向 GitHub 確認」。
- **歷史時間線**：`fixed_by` 轉換顯示「GitHub（代表 Alice 的規則）把狀態從 處理中 改成 已解決・PR #12」；actor 圖示用 GitHub 標誌，不顯示成 Alice 本人。
- **@提及**：留言的 chip 可點（開使用者小卡：名稱、login）；不顯示「對方看不看得到」。

### 6.9 專案設定：修正規則（`/projects/{key}/settings/fix-rules`，`manage_project`）

- **位置**：專案設定的「開發（GitHub）」區塊之下；專案沒有任何連動 repo 時顯示「先連動 GitHub repo 才能使用」並停用。
- **版面**：頂部說明（R-F11 全文，黃底）；規則清單（最多 5 條）：啟用開關、關鍵字（tag 輸入，預填 GitHub 那組）、目標狀態（下拉，只列本專案 tracker 用得到的狀態）、適用 tracker（多選，預設全部）、「規則以〈sponsor 名稱〉的權限執行」灰字；停用原因（`disabled_reason`）用紅字顯示在該列。
- **儲存**：`PUT /projects/{id}/fix-rules`（`lock_version`）；儲存前確認框「儲存後，這些規則會以你的權限執行。」`fix_rule_invalid` 標在對應列。
- **行動版**：每條規則一張卡片。

### 6.10 系統管理：GitHub 設定（既有 `/admin/github` 修改）

- 每個 installation 加「push 事件：最後收到 〈時間〉／從未收到」；從未收到時黃條「GitHub App 還沒訂閱 push 事件，commit 連結與修正關鍵字不會運作」＋說明連結（升級說明）。
- 「可連動」開關的說明加「PR 標題、分支名與 **commit 訊息**會顯示給看得到相關議題的人」（R-C11）。

### 6.11 匯出

- **入口**：列表視圖、搜尋結果頁、專案議題分頁的工具列「⋯ › 匯出 CSV」；專案工時報表與我的工時頁「匯出 CSV」。A4 選 (b) 且沒有權限時選單項目停用＋hover 說明；未登入（匿名）不顯示。
- **對話框**：欄位清單（預設＝目前視圖的欄位，可勾選、拖拉排序）、「包含說明」勾選、說明文字「匯出內容是你目前看得到的資料，最多 10,000 列。檔案用 UTF-8 編碼，可以直接用 Excel 開啟。」＋「匯出」。
- **進行中**：按鈕轉圈（最長 30 秒）；完成後瀏覽器下載（fetch 取得 blob → 建立下載連結）。
- **錯誤**：`export_too_large`「超過 10,000 列，請縮小篩選範圍。」＋「調整篩選」；`query_timeout` 同 05 共通規則；`rate_limited`「請稍候再匯出。」（同時第二個匯出：「你有一個匯出正在進行，請等它完成。」）；串流中斷（超過 50 MB）：「資料量超過 50 MB，請減少欄位或縮小範圍。」，不存不完整的檔案
- **行動版**：同一對話框，欄位清單改成勾選清單（無拖拉）。

### 6.12 匯入精靈（`/projects/{key}/imports/new?kind=issues|time_entries`）

- **入口**：專案議題分頁工具列「⋯ › 從 CSV 匯入」、專案工時報表「匯入」；沒有 `import_*` 權限時不顯示入口（這是整頁功能，不是單一動作）。
- **步驟條**：① 上傳 ② 檔案設定 ③ 欄位對應 ④ 值對應 ⑤ 試跑 ⑥ 執行與結果。可以回到前面的步驟；改了 ②–④ 之後 ⑤ 要重跑。
  1. **上傳**：拖放區＋選檔；說明「CSV 檔，最大 10 MB、5,000 列；第一列必須是欄位名稱」；「下載範本」（只有標題列的 CSV，前端產生）。上傳中進度條。`payload_too_large`、`import_invalid_file` 在拖放區下紅字（`encoding` 時附「改用其他編碼」）。
  2. **檔案設定**：編碼（下拉：UTF-8、Big5、UTF-16LE、UTF-16BE；`encoding_guessed` 時黃條「我們猜這個檔案是 Big5 編碼，請看下面的預覽是否正確」）、分隔符（逗號／分號／Tab）、日期格式；下方前 10 列預覽表格（即時重新解析）。
  3. **欄位對應**：左欄檔案的欄名＋第一列範例值，右欄下拉選 oxledger 欄位（「不匯入」為預設）；自動對應的列標「自動」；必填欄位（主旨）沒對應時「下一步」停用並說明。
  4. **值對應**：每個清單型欄位列出檔案裡出現的不同值（最多 200 個）與對應結果；對應不到的紅色、下拉選一個 oxledger 值或「留空」；使用者欄位對應不到時提示「只能對應到本專案可指派的成員」（工時的使用者：「只能對應到本專案能記工時的人」）。
  5. **試跑**：「開始試跑」→ 進度條（每秒輪詢 `GET /imports/{id}`）→ 結果：「可以建立 4,812 張，有 188 列有問題」＋錯誤列表格（列號、欄位、錯誤文案；可篩錯誤類型）＋「下載問題清單」（report.csv）。選項：「出錯的列略過，其他照常匯入」（唯一模式，B-28 的說明文字）、「直接接受，不進分流」（有 Triage 且有權限才出現）、「通知被指派的人」（預設不勾）。
  6. **執行與結果**：「開始匯入」（確認框「將建立最多 4,812 張議題。執行中可以停止，已建立的不會被刪除。」）→ 進度條＋「停止」→ 完成：成功／失敗／略過數、錯誤列表格、「下載結果」、「查看建立的議題」（開一個條件為「本次匯入」的列表——Filter 欄位 `import_id`，只對擁有者有效）、「撤銷這次匯入」（開對話框選關閉狀態，結果列出「無法撤銷」的列與原因）。
- **離開與回來**：匯入在伺服端持續；「我的匯入」清單（專案議題分頁 ⋯ 裡）列出進行中與 30 天內的匯入，點進去回到對應步驟。
- **狀態**：`expired`「這個匯入已過期（7 天沒有動作），請重新上傳。」；`import_settings_changed`「設定在試跑後改過，請重新試跑。」
- **行動版**：只支援 ①、⑤、⑥（檢視）；②–④ 顯示「欄位對應請在較寬的螢幕上操作」。

### 6.13 通知與偏好

- 通知中心原因標籤加「提及你」（`mentioned`）；摘要「Carol Hsu 在留言中提及你：已在 Safari 17 重現…」（純文字摘要，R-RF7）。
- 通知偏好加一列「有人提及我」（預設開）。

### 6.14 委派精靈與委派頁的修改

- 「操作」清單加「編輯 wiki」「改名或移動 wiki 頁」，標「新操作」；預設模式依 A5。
- 委派開著 `read_own_time` 時〔A6 建議 (b)〕：wiki 兩個操作的模式固定顯示「送審（因為這份委派能讀你的工時）」並停用切換；`read_own_time` 的警告文案加一句「開啟後，這個 agent 改 wiki 都要你核准」。
- 委派專案列表：wiki 狀態徽章「wiki：可讀寫」「wiki：唯讀」「wiki：不可用」（hover 說明 R-AG1 的原因：「這個專案的 wiki 讀者比議題讀者少」或「wiki 模組未開」）。

---

## 7. 測試計畫

編號 T-M2-xx；「E4w」指 02 §9.7 的權限矩陣測試。每個 `not_found` 案例都另外送一份「目標不存在」的同形請求，比對回應（去掉 id 後完全相同），照 05 M1.11 的做法。

### 7.1 Markdown 與參照

| # | 情境 | 預期 |
|---|---|---|
| T-M2-01 | XSS 語料：`<script>`、`<img onerror>`、`<svg onload>`、`javascript:` 連結、`[x](JaVaScRiPt:…)`、HTML 實體編碼的 scheme、`data:text/html`、Markdown 連結標題裡的引號跳脫、巢狀 HTML 註解、`<style>`、表格儲存格裡的 HTML、連結與圖片網址 `//evil.example`、`/\evil.example`、`/attachments/<uuid>/content/../x`、`<input type="text">`（至少 60 個樣本，含 OWASP XSS cheat sheet 的變形） | 第一階段 HTML 不含任何可執行屬性或 scheme；原文標籤以文字顯示；`//`、`/\` 開頭的網址當外部連結、不當本站相對路徑；不符格式的 `img src` 整個移除；`input` 只剩停用的 checkbox；ammonia 前後的 DOM 斷言（不靠字串比對） |
| T-M2-02 | 參照存在性：`AUTH-1`（看得到）、`AUTH-2`（私有，看不到）、`AUTH-999`（不存在）、`ZZZ-1`（專案不存在）各寫在同一段 | 後三者輸出位元組相同（都只是原文）；第一個是連結 |
| T-M2-03 | `@bob`（在可指派清單）、`@eve`（存在但不是成員）、`@nobody`、`@claude-code@alice`、`email@example.com` | 只有 bob 是 chip；eve 與 nobody 輸出相同；後兩者不是提及 |
| T-M2-03a | 提及比對：`@Bob`（只有 login `bob`）、`@Ann`（有 `ann` 與 `ANN` 兩個 login）、`@carol`（成員但不在可指派清單） | `@Bob` 是 bob 的 chip；`@Ann` 輸出原文；`@carol` 輸出原文、不寫提及列 |
| T-M2-04 | `[[KEY:頁]]`：讀得到 KEY wiki 且頁存在／不存在（有、沒有編輯權）、讀不到 KEY wiki、KEY 不存在 | 讀不到與不存在輸出相同；讀得到時依 R-RF4 |
| T-M2-05 | code span、code block、連結文字裡的 `AUTH-1` 與 `@bob` | 不解析 |
| T-M2-06 | agent 寫的留言含 250 字元網址、查詢字串 120 字元網址、一般外部連結；人寫的同樣內容 | agent：前兩個是純文字、第三個帶 `data-ox-agent`；人：三個都是連結 |
| T-M2-07 | 圖片：同容器附件、別的議題的附件、外部網址、`data:` | 只有同容器附件是 `<img>`；其他是「外部圖片」文字連結；前端元件測試確認瀏覽器沒有對外請求 |
| T-M2-07a | 說明寫 `![](attachment:x.png)`，`x.png` 只掛在一則私有留言上；另一則留言引用議題層附件 | 說明不內嵌（「外部圖片」文字）；留言內嵌議題層附件 |
| T-M2-08 | `plain` 舊內容含 `# 標題`、`* 清單`、`AUTH-1`、`<b>` | 原樣顯示（`#`、`*` 不轉格式、`<b>` 是文字）；`AUTH-1` 依 R-RF2 |
| T-M2-09 | 預覽端點：context 是看不到的議題 vs 不存在的議題 | 同一個 `not_found` |
| T-M2-10 | 快取：同內容兩個觀看者（一個看得到 AUTH-2、一個看不到） | 第一階段命中同一快取；第二階段輸出不同；交換順序結果相同 |
| T-M2-11 | 病態輸入：10,000 層 `>`、50,000 個 `*`、200 KiB 單行、1,000 個參照 | 1 秒內完成或回 413；參照超過 200 回 `refs_truncated` |
| T-M2-12 | 搜尋純文字：說明 `**登入**失敗`、`[文件](https://x)` | 「登入失敗」、「文件」命中；`https` 不進索引詞 |
| T-M2-13 | 人核准並套用 agent 提案的說明（含 250 字元網址）；之後人改寫同一段說明 | 套用後照 agent 內容處理（純文字、`data-ox-agent`）；人改寫後是一般連結，編輯時出現「含 agent 寫入的連結」提示 |
| T-M2-14 | 同一頁有說明與兩則留言都用註腳 `[^1]`、都有 `# 標題` | 每個容器的 `fn-`、`h-` id 不重複；註腳連結指到同容器 |
| T-M2-14a | `update_fields` 改 `plain` 說明、不帶 `format`（人、M1 用戶端）；agent 做同樣的事 | 人：仍是 `plain`；agent：`forbidden_op`（`agent_format`） |

### 7.2 提及與通知

| # | 情境 | 預期 |
|---|---|---|
| T-M2-15 | Alice 留言 `@bob`；Bob 讀得到 | Bob 收到 `mentioned` |
| T-M2-16 | 私有留言 `@bob`，Bob 沒有 `view_private_notes` | 沒有通知；Bob 的未讀數不變 |
| T-M2-17 | agent 留言 `@bob` | chip 照顯示；沒有 `issue_event_mentions` 列、沒有通知 |
| T-M2-18 | 說明從 `@bob` 改成 `@bob @carol` | 只有 Carol 收到 |
| T-M2-19 | 提及後 Bob 被移出專案，再讀通知 | 該事件不出現（05 §3.11.3 重驗） |
| T-M2-20 | 匯入（`notify = false`）的說明含 `@bob` | 沒有通知 |
| T-M2-21 | 遮蔽含提及的留言 | 提及列刪除；Bob 的通知讀取時不顯示該事件 |
| T-M2-21a | 遮蔽掛有附件的留言 | 附件 `redacted_at` 有值、blob 清除、下載 `not_found` 形狀同不存在 |
| T-M2-21b | 遮蔽說明時勾選／不勾選議題層附件 | 勾了的被遮蔽；沒勾的照常下載 |

### 7.3 Wiki

| # | 情境 | 預期 |
|---|---|---|
| T-M2-25 | 建立、更新（正確 `lock_version`）、過期 `lock_version`、相同內容 | 版本 1→2；`conflict`；no-op 不產生版本 |
| T-M2-26 | 兩人同時建立同標題（大小寫不同、全形空白） | 一個成功、一個 `wiki_page_exists` |
| T-M2-26a | 標題 `ＡＢＣ` 與 `abc`、`Straße` 與 `STRASSE`；標題或附件檔名含 U+202E、U+200B | 前兩組視為同一標題（`wiki_page_exists`）；含 Cf 的回 `wiki_title_invalid`（`char`）／檔名 `invalid_value` |
| T-M2-26b | 還原與建立同標題並行（唯一索引衝突） | 一個成功、一個 `wiki_page_exists`，沒有 500 |
| T-M2-27 | 改名留轉址，舊標題開頁；新頁占用舊標題 | 轉址到新頁並帶 `redirected_from`；新頁建立後轉址刪除 |
| T-M2-28 | 搬移成環、深度 11 | `invalid_value`（`wiki_cycle`、`wiki_depth`） |
| T-M2-29 | 刪除有子頁的頁不帶／帶 `reparent`；30 天內還原；標題被占後還原 | `wiki_has_children`；子頁上移；還原成功；`wiki_page_exists` |
| T-M2-30 | 受保護頁：有 `edit_wiki_pages` 沒有 `protect_wiki_pages` | `forbidden_op` |
| T-M2-31 | 存在性：讀不到 wiki 的人對真實頁、不存在的頁、模組關閉的專案、看不到的專案的同名頁打 GET、versions、diff、export.md 與每個 wiki op | 全部同一個 `not_found` |
| T-M2-32 | 有 `view_wiki_pages` 沒有 `view_wiki_edits` | 頁可讀；versions／diff `forbidden_op`（他讀得到 wiki，存在性不是祕密） |
| T-M2-33 | 差異：大檔、中文字詞、改動 > 2,000 行 | 正確；`truncated` |
| T-M2-34 | undo：之後沒人改／有人改／前一版被遮蔽 | 回到前一版；`changed_since`；`redacted_source` |
| T-M2-34a | undo 建頁；undo 帶 reparent 的刪除（其中一個子頁之後被別人搬走） | 建立的頁被軟刪除（可還原）；頁還原、沒被搬走的子頁搬回、被搬走的不動並列在預覽 |
| T-M2-34b | 兩個並行的 `wiki_move`（A 移到 B 下、B 移到 A 下） | 序列化（`wiki_settings` 鎖）；後者 `wiki_cycle`，不成環 |
| T-M2-34c | 已刪除頁：一般讀者 GET、`manage_wiki` 讀 `/wiki/deleted/{page_id}`、agent `wiki_get` | `not_found`（同不存在）；可讀；`not_found` |
| T-M2-35 | 鎖：建立頁與 D12 遮蔽同專案並行；5000 頁上限並行建立 | 序列化，不死結；不超過上限 |
| T-M2-36 | wiki 搜尋：讀不到的專案 wiki 含 canary 字 | 搜尋不命中、不影響可見結果的回應形狀（Q-S1 基本契約） |
| T-M2-37 | 寫入後立即搜尋 | `consistent_after` 生效；落後時 `search_lagging` |

### 7.4 agent 與 wiki（E4w 延伸）

| # | 情境 | 預期 |
|---|---|---|
| T-M2-40 | W(P) = A(P)：agent 讀、寫 | 可以 |
| T-M2-41 | W(P) ⊋ A(P)（Reporter 只有 wiki 權限）：agent 讀；寫 | 讀可以；寫 `audience_expansion_forbidden` |
| T-M2-42 | W(P) ⊊ A(P)：agent 對存在的頁、不存在的頁 | 兩者都是 `forbidden_op`（`wiki_not_delegable`），回應相同 |
| T-M2-42b | 議題模組關、wiki 模組開（`holders()` 會算出空集合） | `wiki_audience()` 算出 wiki 讀者；agent 讀得到 wiki；寫 wiki 與任何讀者非空的目的地 `audience_expansion_forbidden` |
| T-M2-42c | `own` 可見範圍的角色（例：Reporter own）被授予 `view_wiki_pages`；其他角色 W = A | W(P) ⊋ A(P)：agent 讀可以、寫 wiki `audience_expansion_forbidden`（用 `holders()` 會誤判成 W = A 而放行——這是回歸測試） |
| T-M2-42a | 委派者有 `view_wiki_edits`，但 H(P) ⊊ A(P)（有人讀得到議題與 wiki、讀不到歷史）：agent `wiki_get` 帶 `version` | `forbidden_op`（`wiki_history_not_delegable`）；不帶 `version` 照常 |
| T-M2-43 | agent 讀 AUTH wiki 後寫 WEB 議題留言（WEB 受眾較寬） | `audience_expansion_forbidden`（R 含 AUTH） |
| T-M2-44 | 政策 direct／review／off、principal 暫停、專案暫停、脈絡失效 | 照 05 矩陣：applied／review:policy／forbidden_op／agent_paused×2／context_unavailable |
| T-M2-45 | agent 寫受保護頁、帶 `attachments`、送 `wiki_delete`／`wiki_protect`／`attach` | `wiki_protected`；`attachments_people_only`；`invalid_request` |
| T-M2-46 | agent 同一頁 10 分鐘寫 11 次 | 第 11 次後 principal 暫停 |
| T-M2-47 | 內建 AI：讀 wiki 後寫留言（W = A）；讀 `time_list` 後寫 wiki | 前者照政策；後者 `review:time_taint` |
| T-M2-47a | 外部 agent 的委派開了 `read_own_time`、政策 direct：`wiki_put` | `review:time_read_delegated`（依 A6 建議） |
| T-M2-48 | 既有委派升級後 | 沒有 wiki op；委派頁顯示「新操作」 |
| T-M2-49 | 角色權限改變（`view_wiki_pages` 授給新角色） | 專案 epoch bump，讀過該專案的脈絡 `context_unavailable` |

### 7.5 附件〔A2〕

| # | 情境 | 預期 |
|---|---|---|
| T-M2-50 | 上傳 HTML 偽裝成 `.png`、SVG、PDF、真 PNG | 前三者 `application/octet-stream`＋`attachment`；PNG `inline` |
| T-M2-51 | 下載標頭 | `nosniff`、`CSP: sandbox`、`no-store`、`filename*` |
| T-M2-52 | 存在性：看不到的議題的附件、私有留言的附件（沒有 `view_private_notes`）、不存在的 id | 同一個 `not_found` |
| T-M2-53 | 別人的 upload id、過期的、別專案的、不存在的放進命令文件 | 同一個 `invalid_value`（`upload_unknown`） |
| T-M2-54 | 同專案兩次上傳相同內容；不同專案上傳相同內容 | 同專案一個 blob；不同專案各一個 |
| T-M2-55 | 遮蔽附件，同專案另一張私有單有相同內容的附件 | 兩個都無法下載；確認框不顯示數量；epoch bump |
| T-M2-56 | GC：刪除後 29 天、31 天；upload 過期 | 29 天還在；31 天清除；暫存清除 |
| T-M2-57 | 提交前程序中斷（檔案已寫、DB 沒寫） | GC 清掉孤兒檔（掃目錄比對 blob 表，每日一次） |
| T-M2-58 | 遮蔽附件後，同一人與另一人重新上傳相同內容並掛上 | 上傳回應與一般上傳形狀相同；建立新的 blob 世代；新附件可下載；舊附件仍「內容已遮蔽」 |
| T-M2-59 | 上傳限制：匿名上傳；第 4 個同時上傳；超過每日配額；磁碟低於門檻（假檔案系統）；`Content-Length` 超過上限；body 比 `Content-Length` 長 | 401；429；413；507；413 且沒有讀 body；中止、不留暫存檔；記憶體用量不隨檔案大小成長 |

### 7.6 Git 與 `fixed_by`

| # | 情境 | 預期 |
|---|---|---|
| T-M2-60 | push 到 feature 分支，訊息含 AUTH-1、WEB-2（repo 只連 AUTH） | 存 commit；只有 AUTH-1 有 ref；`on_default_branch = false` |
| T-M2-61 | 同 commit 之後以 `distinct = false` 出現在推到預設分支的 push（fast-forward 合併）；以及預設分支 push 中 `distinct = false` 的 commit 帶 `fixes AUTH-1` | `on_default_branch = true`、不重複 ref；修正關鍵字照 R-F1 (b) 處理（依 A3 是 `suggested`（`direct_push`）或進確認流程） |
| T-M2-62 | 沒有連動的 repo 的 push；tag push；`deleted = true` | `ignored`，不存 |
| T-M2-63 | push 的 commits 達到 2048 筆上限 | 改用 compare 補抓；fake API 驗證呼叫 |
| T-M2-64 | 解除連動與 push 處理並行（兩種順序） | 都沒有殘留 ref；讀取端不顯示（同 T-G17） |
| T-M2-65 | commit 隱藏、撤除；撤除後 commit 沒有其他 ref | 只影響本專案；message 變 `[已遮蔽]`、`redacted_at` 有值；墓碑一列；epoch bump |
| T-M2-65a | **撤除後再 push 同 sha**（另一分支、force push 回來、重送原 delivery body 換新 id）；30 天後清除排程跑過再 push | 本專案不再建 ref、不建 fix link；message 仍是 `[已遮蔽]`；墓碑與 `redacted_at` 列都還在 |
| T-M2-65b | commit 作者欄填別人的 username、由 bob 推送 | `author_login` 照存、畫面標未驗證；`sender_login = bob` |
| T-M2-66 | 存在性：hide 只對到看不到的單的 commit vs 不存在的 commit | 同一個 `not_found`（同 T-G29） |
| T-M2-67 | PR 合併進 main、標題 `fixes AUTH-1, AUTH-2`，規則啟用、sponsor 有轉換權 | API 確認後兩張轉成目標狀態；事件 origin `integration`、`on_behalf_of = sponsor`；payload 無標題 |
| T-M2-68 | PR 合併進 `release` 分支；push 到 feature 分支含 `fixes` | `skipped`／不偵測（`not_on_default_branch`） |
| T-M2-68a | 直接 push 到預設分支、訊息 `fixes AUTH-1`、規則啟用 | A3 選 (i)：`suggested`（`direct_push`），不呼叫 API、不轉換；選 (ii)：進確認流程 |
| T-M2-68b | 專案有連動但沒有符合的啟用規則 | 直接 `suggested`（`no_rule`）；fake API 沒有被呼叫 |
| T-M2-69 | **重放**：攔截的合法 `pull_request` body 換新 delivery id 重送；偽造「merged」但 API 回 not merged | 第一個不產生第二次轉換（唯一索引＋`already_closed`）；第二個 `skipped`（API 確認失敗） |
| T-M2-70 | sponsor 沒有那條 workflow 轉換；sponsor 被鎖定；目標狀態有必填 | `suggested`（`workflow_disallowed`）；規則停用＋`suggested`（`sponsor_inactive`）；`suggested`（`form_rule`） |
| T-M2-70a | sponsor 看不到這張單（私有單）vs sponsor 沒有轉換權 | 兩者都是 `workflow_disallowed`，開發卡的回應去掉 id 後位元組相同 |
| T-M2-70b | 規則因 sponsor 失權自動停用；同一事件重送 | `project_setting_events` 一列（`actor_id`＝系統 principal，有 `operation_request_id`）；重送不產生第二列 |
| T-M2-71 | 單已關閉、在 Triage pending、51 張單 | `already_closed`、`in_triage`、全部 `too_many` |
| T-M2-71a | 一次 push 含 30 個 commit、共比對到 250 張單；GitHub API 呼叫次數 | fix link 200 列、`fix_links_truncated`；compare 只呼叫 1 次（fake server 計數） |
| T-M2-72 | 同一修正同時出現在 PR 合併與 squash commit 的 push | 只轉換一次（第二次 `already_closed`） |
| T-M2-73 | agent 鏈：agent 連寫 2 次後 fixed_by 轉換，再 agent 寫 | 鏈不重設、fixed_by 不計入（05 §3.12.4 的計數不變） |
| T-M2-74 | 整合寫入後的 undo | 有權限的人「轉回 X」成功；undo 路由對該 execution 回 `undo_not_applicable` |
| T-M2-74a | sponsor 用「最近寫過」篩選（`Col::Wrote`） | 只被 `fixed_by` 轉換過的單不出現 |
| T-M2-75 | GitHub API 錯誤 5 次 | `skipped`（`unconfirmed`）；不轉換 |
| T-M2-76 | 日誌 | 不含 PR 內文、commit 訊息原文、secret、token（T-G16 擴充） |
| T-M2-77 | 存在性：讀不到議題的人看 `/projects/{id}/dev?filter=fix_skipped` | 不出現、不透露數量 |
| T-M2-78 | webhook 並行：同時 17 個請求，處理器被卡住 | 第 17 個立即 503（不排隊），前 16 個照常完成 |

### 7.7 CSV 匯出

| # | 情境 | 預期 |
|---|---|---|
| T-M2-80 | 公式注入：主旨 `=HYPERLINK(...)`、`+1`、`-cmd`、`@SUM`、`＝1`、Tab 開頭；數字欄 `-5` | 文字欄前加 `'`；數字欄不加 |
| T-M2-81 | 受限自訂欄位：有權限與沒權限的兩人匯出同一視圖；`columns` 指定讀不到的欄位與不存在的欄位 | 沒權限的人沒有該欄；兩種指定回同一個 `unknown_field` |
| T-M2-82 | 私有單、私有留言含 canary | 沒權限的人的檔案沒有 canary；不匯出留言 |
| T-M2-83 | 10,001 列 | `export_too_large`，不含實際數量 |
| T-M2-84 | 中文、換行、引號、逗號 | Excel 相容：BOM＋CRLF＋RFC 4180（用 `csv` crate 讀回比對） |
| T-M2-85 | 稽核 | `export_events` 一列，條件只有 HMAC（換 server secret 後同條件得到不同值） |
| T-M2-86 | agent token、匿名打匯出端點與 `export.md` | 401 |
| T-M2-86a | 匯出 10,000 列、每列很大（超過 50 MB）；同一人同時兩個匯出 | 串流中斷、`truncated = true`、伺服器記憶體不隨檔案成長；第二個 429 |

### 7.8 CSV 匯入

| # | 情境 | 預期 |
|---|---|---|
| T-M2-87 | UTF-8、UTF-8 BOM、UTF-16LE BOM、Big5（Excel 繁中存檔樣本）、亂碼 | 前四者正確；Big5 標 `encoding_guessed`；亂碼 `import_invalid_file`（`encoding`） |
| T-M2-88 | 分號、Tab 分隔；引號內換行 | 正確 |
| T-M2-89 | 指派對應：可指派成員、存在但不是成員、不存在、重名 | 第一個 ok；中間兩個同一個 `unknown_value`；重名 `ambiguous_value` |
| T-M2-90 | **存在性總表**：本文所有新路由與新 op（wiki、附件、匯入、commit hide、fix rules、匯出的 view_id、預覽 context）由沒權限的已登入使用者用真實 id 與不存在 id 各打一次 | 兩個回應去掉 id 後完全相同；沒有寫入（擴充 `t_m1_66_http_m1_routes_for_a_person_without_rights` 與 `t_m1_66_existence_oracles_beyond_the_matrix`） |
| T-M2-91 | 試跑後執行：中途有一列 workflow 不允許、有一列 L1 必填沒填、規則在執行中發布新版 | 那幾列 `error`（`workflow_disallowed`、`required_field_missing`、`form_rules_changed`），其他列建立 |
| T-M2-92 | 執行到第 300 列時程序被殺，重啟 | 從第一個 `pending` 列繼續；沒有重複的單（冪等鍵） |
| T-M2-93 | 匯入者沒有 `add_issues`（只有 `import_issues`） | 每列 `forbidden_op` |
| T-M2-94 | 撤銷：之後有人改過其中 3 張 | 其他關閉；3 張列在「無法撤銷」 |
| T-M2-95 | 列錯誤報表 | 只有列號、碼、欄位 key；不含儲存格內容（canary 不出現） |
| T-M2-96 | CHECK 聯集 | 所有重建的 CHECK 值集合 = 程式常數（T-M1-124 擴充） |
| T-M2-97 | 專案 D12 遮蔽時有未清的匯入暫存檔 | `imports.file` 變 NULL |
| T-M2-98 | 工時匯入：記到別人名下（有／沒有 `log_time_for_other_users`）、看不到的議題 key、對應到本專案不能記工時的成員 | ok／`forbidden_op`；看不到的 key 與不存在同一個 `unknown_value`；不能記工時的成員也是 `unknown_value` |

### 7.9 發布門檻（M2.9）

- E4w：§3.11 的新 op × 05 M1.11 的 13 種身分矩陣全套；越權、繞過政策、重複效果、被拒仍有效果 = 0。
- canary 洩漏 = 0：Markdown 渲染（參照 title、提及、wiki 連結）、預覽、通知摘要、wiki 搜尋、匯出、匯入報表、附件中繼資料、開發卡（commit 與 fix link）、內建 AI 的 wiki 工具結果。
- XSS 語料（T-M2-01）全過；前端元件測試：`{@html}` 只出現在 `<RichText>`（lint）。
- webhook：T-M2-69（重放）、T-G1–T-G36 全部重跑。
- 升級測試：從 0030 的資料庫（**M1 發布版**；改號後要經過 `0031_p2_api_gaps.sql`，它只加欄位、不動權限與 epoch）（含 agent 委派、通知、GitHub 連動）升到 0037：既有 agent token 照常、既有說明顯示不變（`plain`）、**每個專案的 epoch 不變、既有工作脈絡不會 `context_unavailable`**（B-37 的前提）、`wiki_settings` 每個專案一列、搜尋藍綠重建完成。

**哪些只在測試裡驗證**：GitHub 的 push、compare、PR API 在 CI 只用假伺服器；**真實 GitHub App 加訂 push 後的事件形狀、commits 上限、compare 回應**要在 M2.6 用一個測試 repo 手動跑一次並記錄；Excel（Windows 繁中）開啟匯出檔、存 Big5 CSV 再匯入，要手動驗一次；瀏覽器實際 CSP 與附件下載標頭要在真瀏覽器確認。

---

## 8. 分階段實作順序

| 階段 | 內容 | 依賴 | 完成標準 |
|---|---|---|---|
| **M2.0** 前置 | 新 crate 由使用者核可後加入（§9.2 的新依賴表，D17）並過 `cargo deny`；0032；`perm.rs`；`format` 欄位進命令引擎、describe、事件、undo；`issue_event_mentions` 寫入；origin `integration` 的 CHECK；`/agent-contexts*` 移除 | M1 完成 | 既有測試全綠；T-M2-96；OpenAPI 與 `api/types.ts` 重產；升級說明草稿 |
| **M2.1** Markdown | `markdown.rs`（comrak＋ammonia、AST 走訪、佔位節點、第二階段解析、快取、純文字抽取）；`*_html` 欄位；`/markdown/preview`；提及派送（`notify` 加 `mentioned`、偏好）；議題索引改純文字＋layout +1；前端 `<RichText>`、`<MarkdownEditor>` | M2.0 | T-M2-01…21；E1 nDCG@10 ≥ 0.810；前端議題頁可用 |
| **M2.2** Wiki 核心 | 0033（含 `wiki_settings` 回填與 trigger）；wiki op 與鎖序；人的 wiki undo 新路徑（R-W9）；已刪除頁讀取；歷史、差異（`similar`）、回復、undo；轉址；刪除與還原；保護；起始頁；D12 的 wiki 目標；專案分頁 `wiki`；REST | M2.1 | T-M2-25…35（含 26a、26b、34a–34c）；前端 §6.3–§6.6 可用 |
| **M2.3** Wiki 搜尋與 agent | wiki 索引與 consumer；`search kinds`；`audience.rs` 新增 `wiki_audience()`；MCP `wiki_list`、`wiki_get`、wiki op 進 `command_submit`／提案；R-AG1–R-AG11；政策與範本；內建 AI 工具 | M2.2 | T-M2-36、37、40…49（含 42b、42c、47a）；**02 E4w、§9.6 受眾測試、05 T-M1-66 矩陣全部重跑全綠** |
| **M2.4** 附件〔A2〕 | 0034；儲存、上傳、`attach`／`delete_attachment`、下載標頭、GC、R-M4 圖片、D12 目標 | M2.2（wiki 附件）；可和 M2.3 並行 | T-M2-50…59；T-M2-07、07a、21a、21b；前端附件卡與編輯器附檔 |
| **M2.5** Git push 與 commit | 0035；`push` 分派、`default_branch`、commit 與 ref（不論 distinct 的預設分支判斷）、compare 補抓、hide、revoke 擴充（撤除墓碑、`redacted_at`）、保存；webhook load shed；開發卡 Commits；admin 頁 push 狀態 | M2.0（可和 M2.1–M2.4 並行，改號規則見 §2.1） | T-M2-60…66（含 65a、65b）、T-M2-78；T-G1–T-G36 全綠 |
| **M2.6** `fixed_by`〔A3〕 | 0036；偵測、API 確認工作、執行（origin `integration`）、提示、規則 API 與設定頁、停用邏輯 | M2.5 | T-M2-67…77（含 68a、68b、70a、70b、71a、74a）；**用真 GitHub App 與測試 repo 手動跑一次 push＋PR 合併＋fixes**，記錄結果 |
| **M2.7** CSV 匯出 | 0037 的 `export_events`；`/exports/*`；公式跳脫；稽核 API；前端匯出對話框 | M2.1（說明原文） | T-M2-80…86；Excel 手動開檔一次 |
| **M2.8** CSV 匯入 | 0037 其餘；上傳、編碼／分隔偵測、對應、試跑、worker、報表、撤銷、通知選項；Filter 欄位 `import_id`；前端精靈 | M2.7（共用格式化）；M2.1 | T-M2-87…98；5,000 列匯入本機 ≤ 5 分鐘；Big5 樣本手動驗一次 |
| **M2.9** 發布門檻 | §7.9 全部；`docs/impl/upgrade-notes.md` 0032–0037 | 全部 | 越權、繞過政策、重複效果、canary 洩漏、XSS = 0；升級測試通過 |

M2.5–M2.6（Git）和 M2.1–M2.4 不同表、不同模組，可以並行；M2.3 會改 agent 讀取範圍與 `audience.rs`，和 M2.6 的命令引擎改動（origin `integration`）要排先後合併（建議 M2.3 → M2.6）。前端可從 M2.1 起接真資料。

---

## 9. 設計決策

### 9.1 A 類：使用者已決定（影響產品或安全）

審查後合併成 6 題：原 A1 併入圖件的範圍缺口；A3 加「觸發來源」維度；A5 加選項 (c)；新增 A6（工時例外延伸到 wiki）。

**2026-11-13 使用者決定**：原話「依照建議進行」。A1–A6 全部採「建議」欄：A1 (a)、A2 (a)、A3 (a)＋(i)、A4 (a)、A5 (c)、A6 (b)。正文中標〔依建議預設 Ax，待確認〕的地方都視為已決定。依 A1 (a)，01 §14.8 原排在 M2 的圖件改到 P5（01 的文字在 P5 契約時一併改，見 §9.4）。

| # | 問題 | 選項 | 建議 | 代價 |
|---|---|---|---|---|
| **A1** | **範圍切分**：有兩群原本排在「M2」的東西本契約沒有涵蓋，要放哪裡。(1) 05 延到 M2 的其他項目：email／SMTP 與 digest、品牌 U28–U30、設計交接 U25、內建 AI 角色化 U37、`view.render`、L3 專案總覽、替別人加關注、`contains` 篩選、概念標註、AI key 存資料庫。(2) **01 §14.8 與研究 04 排在 M2 的圖件**：自由心智圖圖件、BPMN 圖件的檢視與編寫、圖件版本與連結權限 | (a) 本契約只做研究 §8 的五項（Markdown、Git webhook、`fixed_by`、wiki、CSV）＋直接帶出的（@提及、工時 CSV、附件、`/agent-contexts*` 移除）；(1) 與 (2) 一起另開 **P5 契約**，並把 01 §14.8 的 M2 列改成 P5；(b) 全部併進本契約；(c) 本契約同 (a)，**圖件另開 M2b 契約**（仍算 M2，排在本契約的 wiki 與附件之後，因為圖件版本與連結權限要沿用 wiki 的版本模型與附件儲存），(1) 去 P5 | **(a)** | (a)：01 §14.8 原本承諾 M2 有心智圖與 BPMN 圖件，要改那份文件；不過 00 的 U27（2026-11-07）已把視圖順序改成 list／board／triage → roadmap＋健康度 → Gantt → mindmap → BPMN，而 Gantt 在 01 排 M3，照 U27 圖件本來就該在 Gantt 之後。@提及只有站內通知、沒有 email；設計交接延後。(b)：本契約再大一倍以上——email 要選 SMTP 依賴與退訂設計，品牌要新的點陣化元件（U30），圖件要新的前端繪圖元件（01 §14.9 的候選都還沒查證）與圖件權限模型；審查與實作週期明顯拉長。(c)：保住「M2 有圖件」的承諾，但多一份契約與一輪審查，而且圖件會排在 Gantt 前面，和 U27 衝突，要先改 U27 |
| **A2** | **附件**要不要在 M2 做 | (a) 做最小版（§3.6）：議題、留言、wiki 可附檔；存在伺服器本機目錄、專案內依內容去重；只有本站圖片內嵌、SVG 一律下載；agent 只看得到中繼資料；每人每日 1 GB、同時 3 個上傳、磁碟低於門檻就拒收；匿名不能上傳；不掃毒。(b) 不做：Markdown 圖片一律不載入，wiki 只有文字 | **(a)** | (a)：多了檔案儲存與備份責任（`OXLEDGER_FILES_DIR` 要和 PostgreSQL 同一時間點備份）；上傳是新的攻擊面（由 R-A2、R-A3、R-A6 處理）；沒有掃毒，惡意檔只能靠「非圖片一律下載」降低風險；管理者要設定配額與磁碟門檻。(b)：議題與 wiki 沒辦法放截圖，Redmine 使用者會覺得少了基本功能；U25 設計交接卡沒有「有版本的基準檔」 |
| **A3** | **`fixed_by`：commit 或 PR 寫了 `fixes AUTH-123` 時，要不要自動改議題狀態、用誰的身分、認哪些來源**（推翻 U45b「合併不自動改單」） | **身分**：(a) 專案管理者設定規則（預設關閉），以**設定規則的人（sponsor）的權限與 workflow** 執行，歷史顯示「GitHub（代表 Alice 的規則）」；(b) 把 GitHub 帳號連到 oxledger 使用者（要新的 GitHub 使用者 OAuth 連結流程），以合併 PR 的人的權限執行，對不到人就只提示；(c) 維持 U45b：永遠只提示，人按一下轉換。**觸發來源**（選 (a) 或 (b) 時）：(i) 只認 **PR 合併進預設分支**，直接推到預設分支的 commit 寫了關鍵字只提示；(ii) 也認**直接 push 到預設分支**的 commit | **(a)＋(i)** | (a)：任何能把 PR 合併進預設分支的人，都能讓本專案符合規則的單轉成目標狀態（設定頁明講，R-F11）；sponsor 離職、被鎖或失去管理權時規則自動停用（R-F8）；**PR 內文與 commit 訊息可能出自外部貢獻者**（fork 來的 PR）——關鍵字是外人寫的、按合併的是維護者，維護者要看清楚。(b)：權限最貼近真人，但多一套帳號連結，而且合併的人常常不是 oxledger 成員。(c)：沒有風險，但 Redmine 常用的「關鍵字關單」就沒有。(i)：直接推上預設分支的修正要人按一下「轉換狀態」；squash、rebase 合併仍經「PR 合併」觸發，不受影響。(ii)：直接 push **繞過 PR review**，有推送權的人（含外洩的 token、CI 機器人）不經任何審核就能推進狀態；repo 沒開分支保護時最明顯 |
| **A4** | **CSV 匯出要不要獨立權限** | (a) 讀得到就能匯出（Redmine 的做法【推論：Redmine 沒有匯出權限，看得到列表就能匯出 CSV】），加稽核紀錄、10,000 列與 50 MB 上限、每人同時一個；(b) 新權限 `export_issues`（升級時授給所有持有 `view_issues` 的角色，之後管理者可以收回）。兩案都一樣：匿名不能匯出、agent 不能匯出 | **(a)** | (a)：匯出內容和畫面一樣，限制匯出擋不住複製貼上或截圖，只換來管理負擔；但大量帶走資料比較容易，只能事後查稽核。(b)：可以讓「外包、訪客」這類角色看得到但不能整批帶走；多一個權限要管 |
| **A5** | **agent 寫 wiki 的新委派預設** | (a) direct：直接寫入（D20 原則）；(b) review：每次都送人核准（同 05 A1(b) 工時的做法）；(c) direct，另在專案活動頁與首頁「最近」區塊顯示「agent 最近 7 天改了哪些 wiki 頁」（讀得到該 wiki 的人才看得到，§6.7） | **(c)** | 三案共同：**wiki 沒有通知**（B-12），agent 直接改了 wiki 不會有任何人收到提醒。(a)：wiki 讀者通常比單張議題多，agent 被注入時一次寫入影響面大，而且可能很久沒人發現；有版本歷史與 undo（R-W9）可回復，寫入範圍已被 R-AG1／R-AG3 限在 W = A 的專案。(b)：每次 agent 改文件都要人核准，「讓 AI 整理 wiki」會很慢，也和 D20「預設直接寫入」不一致。(c)：保留 direct 的流暢，多一個前端區塊與一個查詢；仍然不是通知，沒去看首頁或活動頁的人一樣不知道 |
| **A6** | **工時例外要不要延伸到 wiki**：05 A1(c) 讓外部 agent 在委派開了 `read_own_time` 時讀**委派者本人的工時**（含說明）——這是受眾一致保證的已知例外（工時讀者比議題讀者少，agent 把工時說明抄進一般內容時攔不住）。M2 讓 agent 能寫 wiki，這個例外就延伸到 wiki：agent 讀了工時說明再寫進 wiki，讀者從「工時讀者」變成整個 wiki 讀者 | (a) 接受，只更新委派精靈的 `read_own_time` 警告文案（加「也可能被寫進 wiki」）；(b) 開了 `read_own_time` 的委派，`wiki_put`／`wiki_move` 不論政策**一律送審**；(c) 開了 `read_own_time` 的委派**不能寫 wiki**（`forbidden_op`） | **(b)** | (a)：最簡單；但 wiki 是全專案的長期文件，外洩面比一則留言大，又沒有通知（A5），抄進去的工時說明可能很久沒人發現。(b)：server 不記外部 agent 讀過什麼（02 §3.8 M2），只能對整份委派一律送審——個人 agent 的 `read_own_time` 預設開（05 B-22），所以**個人 agent 改 wiki 預設都要委派者核准**，除非委派者關掉 `read_own_time`；內建 AI 已有 time_taint（只在讀過工時的那次對話之後才送審），不受影響。(c)：最安全；但個人 agent 預設開 `read_own_time`，等於個人 agent 預設不能寫 wiki，使用者要在「整理工時」和「寫 wiki」之間二選一 |

### 9.2 照預設做的決定（之後進 `decisions.tsv`）

| # | 決策 | 預設 | 理由 |
|---|---|---|---|
| B-1 | Markdown 渲染在哪裡 | 伺服端（comrak＋ammonia），前端只插 `*_html` | 一份消毒器；參照解析需要權限，只有伺服端做得到；MCP 不需要 HTML |
| B-2 | HTML 透傳 | 不透傳，原文標籤跳脫成文字顯示 | 安全；作者看得到自己寫了什麼 |
| B-3 | `#123` 語法 | 不支援，只認 `KEY-123` | D9；和 Markdown 標題、GitHub `#PR` 衝突 |
| B-4 | 工時說明、版本說明、自訂欄位的 Markdown | 維持純文字 | 短欄位；少一個渲染面 |
| B-5 | 舊內容格式 | 保留 `plain`，改寫時才轉 | 不突然改變舊資料外觀 |
| B-6 | 外部圖片 | 不載入，顯示成文字連結；不做圖片代理 | 隱私與資料外送；代理會引入 SSRF |
| B-7 | agent 內容的外部連結 | 套 05 §6.19 規則（長網址不成連結、點擊確認） | 05 附錄 D V23 |
| B-8 | 提及外觀與通知的判斷 | 「是不是本專案可指派清單裡的人」（和自動完成同一個集合），不看「對方讀不讀得到」 | 後者是權限 oracle；和自動完成一致，使用者看得懂（審查 M5） |
| B-9 | 既有專案的 wiki 模組 | 不自動開；新專案預設開 | 不讓舊專案突然多出一個可寫的地方；不用動 epoch |
| B-10 | wiki 逐頁讀取權限 | 不做 | Redmine 也沒有；逐頁 ACL 會讓 W(P) 每頁不同，agent 規則變複雜 |
| B-11 | wiki 上限 | 5000 頁／專案、10,000 版／頁、512 KiB／版 | 防濫用；超過的很少見 |
| B-12 | wiki 關注與通知 | 不做（P5 和 email 一起） | 通知列目前只綁議題（`notifications.issue_id`） |
| B-13 | wiki 搜尋 | 獨立索引、結果分開回 | 不動 issue 的 layout；兩種分數不可比 |
| B-14 | wiki 跨專案搬頁 | 不做 | 受眾會改變，需要另一套檢查 |
| B-15 | 附件上傳格式 | 原始 body，不用 multipart | 不需新 crate（axum multipart 會帶進 `multer`） |
| B-16 | 附件下載網域 | 同源；可選 `OXLEDGER_FILES_ORIGIN` | 同源加 `CSP: sandbox`、`nosniff`、非圖片一律 `attachment` 已足夠；需要更嚴時再設 |
| B-17 | 掃毒 | 不做，留掛點 | 沒有選定的掃毒元件；選 ClamAV 之類要另外問 |
| B-18 | 附件內容判斷 | 只看檔頭；只有 4 種圖片 inline | 不信任副檔名與 Content-Type |
| B-19 | agent 與附件 | 只讀中繼資料 | 內容可能是任意格式，抽文字要新元件；上傳是新的寫入通道 |
| B-20 | commit 作者資訊 | 只存 GitHub username（標「未驗證」）與推送者 `sender.login`，不存 email、姓名；網址讀取時組，不存 payload 的 URL | 個資最小化；commit 作者欄可任意填 |
| B-21 | push 對帳 | 不做，只靠補送 | GitHub 沒有權威的「最近 push」清單【推論】 |
| B-22 | commit 記工時（Redmine `@2h`） | 不做 | commit 作者身分不可驗證 |
| B-23 | `fixed_by` 掃 PR 內文 | 掃（只找修正關鍵字，不存） | GitHub 自己的關鍵字慣例就寫在內文 |
| B-24 | `fixed_by` 目標狀態的建議 | 非終態（已解決） | 誤關的代價較低 |
| B-25 | 匯出留言 | 不匯出 | 私有留言與可見性規則太複雜；需要時另案 |
| B-26 | 匯出編碼 | 只有 UTF-8 BOM | Excel（含繁中 Windows）開得了；少一個選項 |
| B-27 | 匯入更新既有議題 | 不做，只建新單 | 批次覆寫風險高；需要時另案（要 precondition 設計） |
| B-28 | 匯入失敗處理 | 逐列成敗，不做全有全無 | 5,000 列一個交易會長時間鎖、擋 outbox frontier（D11）；有試跑與撤銷 |
| B-29 | 匯入的通知 | 預設不通知 | 避免一次灌爆通知中心 |
| B-30 | 介面角色捷徑（U24） | PM、設計師加「Wiki」；工程師加「開發」 | 研究 04 §1.1 的常看畫面 |
| B-31 | 匯出上限 | 10,000 列，站台可調 | Redmine 預設 500【推論：Redmine `issues_export_limit` 預設 500】；我們用串流與單一快照，可以放寬 |
| B-32 | webhook 並行上限 | 16，滿了立刻 503（load shed，不排隊） | 未驗證端點的資源保護；排隊一樣會耗盡連線；補送會找回 |
| B-33 | `/agent-contexts*` | M2.0 移除 | 05 B-43 已排定 |
| B-34 | Markdown 渲染快取 | 記憶體 LRU、只快取第一階段 | 第二階段依觀看者，不能共用 |
| B-35 | agent 寫的提及 | 渲染但不通知 | R-MN4 |
| B-36 | 新角色範本「文件維護」 | 預設 review | 範本給團隊共用的 bot，比個人委派保守（05 B-22 的精神） |
| B-37 | 0032 授予 wiki 權限時要不要 bump epoch | **不 bump**（暫停 `role_permissions_epoch`；和 0023 刻意 bump 一次不同） | 新權限不改既有內容的讀者集合、升級前沒有 wiki 內容、R-AG1 在讀取當下計算（§2.1）；bump 的代價是所有 agent 脈絡升級後都要重開。前提由升級測試斷言（§7.9） |
| B-38 | agent 內容被人改寫或複製後 | 改寫後算人的內容（編輯器提示「含 agent 寫入的連結」）；複製到別處不追蹤 | 追蹤複製要做內容指紋，成本高、誤判多；剩下的風險是「人把 agent 的連結原樣貼到別處」（審查 M13 的已知限制） |
| B-39 | 提及的 login 比對 | 先精確；不分大小寫只在命中一人時才算 | 0001 的 login 是區分大小寫唯一（審查 M6；附錄 B C2 結案） |
| B-40 | 匯出的資源上限 | 逐列串流、10,000 列、50 MB、30 秒、每人同時一個、每分鐘 5 次 | 防止匯出拖垮伺服器（審查 M11） |
| B-41 | 上傳的資源上限 | 單檔 20 MB、每人每日 1 GB、同時 3 個、未用暫存 200 MB、磁碟剩餘 < 5 GB 拒收；匿名不能上傳；邊收邊寫、不進記憶體 | 防止上傳耗盡磁碟或記憶體（審查 M12） |
| B-42 | commit 撤除後再出現 | 持久墓碑 `project_commit_revocations`；`redacted_at` 的 message 不被覆寫；遮蔽過的 commit 列不清除 | 不然再 push 同 sha 就會讓撤除失效（審查 H2） |
| B-43 | `fixed_by` 的 GitHub API 用量 | 每個 PR 或每次 push 只確認一次；每次最多建 200 列 fix link | 避免單一 push 打爆 API 配額（審查） |
| B-44 | 匯出稽核的條件 | 存 HMAC（server secret），不存 sha256 | 條件值空間小，純 hash 可被字典還原 |
| B-45 | 遮蔽說明時的議題層附件 | 對話框讓管理者勾選，預設不勾；遮蔽留言則連帶遮蔽它的附件 | 附件和說明不一定相關；留言的附件屬於那則留言（審查 M2） |
| B-46 | 遮蔽附件後有人重新上傳同內容 | 照常成功、建新的 blob 世代、回應和平常相同 | 擋或提示都會變成「這內容被遮蔽過」的 oracle（審查 M3） |
| B-47 | sponsor 看不到要轉換的單 | `suggested`，原因碼和文案同 `workflow_disallowed` | 不透露 sponsor 的可見範圍 |
| B-48 | 有連動但沒有符合的啟用規則 | 直接記 `suggested`（`no_rule`），不呼叫 API | 只是提示，不需要確認；也修正第一版 R-F3 與 R-F6 的矛盾 |
| B-49 | wiki 索引的 outbox consumer | 照 0018 的世代命名 `wiki@<uuid>`、state 與藍綠；消費全部 outbox 列 | 不卡住 outbox GC；和議題索引同一套維運 |
| B-50 | own 可見範圍角色的 wiki 權限 | 升級時不自動授予 | 授了會讓 W(P) ⊋ A(P)，agent 寫不進 wiki；要的話管理者自己加 |
| B-51 | wiki 標題比對 | NFKC_Casefold；標題與附件檔名禁 Cc、Cf | 全形／半形、大小寫視為同名；擋雙向控制字元的偽裝 |
| B-52 | 系統管理員關閉 repo 的 `linkable` | 只擋新連動，既有連動照收 | 和既有行為一致；要停止接收用解除連動（審查更正 R-C11） |
| B-53 | 更新說明時 `format` 省略 | 保留原格式 | 舊用戶端不會把 `plain` 默默轉成 Markdown |
| B-54 | Markdown 的 id | 標題錨點與註腳 id 加容器前綴 | 同一頁多個容器時不撞名 |
| B-55 | 已刪除的 wiki 頁 | 一般讀者 `not_found`；只有 `manage_wiki` 能讀（還原前檢視）；agent 讀不到 | 刪除的內容不該繼續流通，但管理者要能判斷要不要還原 |
| B-56 | undo 建頁 | 軟刪除（30 天可還原） | 不硬刪，和 D-M2-9 一致 |
| B-57 | 工時匯入的使用者候選 | 本專案能記工時的人 | 可指派的人不一定能記工時 |

**新依賴（D17，要使用者核可後才加入；本輪沒有下載或安裝）**：

| crate | 版本（crates.io，2026-11-12 查；審查已核對版本與維護者） | 授權 | 用途 | 維護者 | 所在地（依公開資料；`unicode-normalization` 仍是推論） |
|---|---|---|---|---|---|
| `comrak` | 0.59.0 | BSD-2-Clause | Markdown 解析與渲染 | Asherah Connor（kivikakk）；共同 owner Garen Torikian（gjtorikian）、digitalMoksha | 澳洲；美國 |
| `ammonia` | 4.2.4 | MIT OR Apache-2.0 | HTML 允許清單消毒 | Michael Howell（notriddle）＋ rust-ammonia 團隊 | 美國 |
| `csv` | 1.4.0 | Unlicense OR MIT | CSV 讀寫 | Andrew Gallant（BurntSushi） | 美國 |
| `encoding_rs` | 0.8.44 | (Apache-2.0 OR MIT) AND BSD-3-Clause | Big5、UTF-16 解碼 | Henri Sivonen（hsivonen） | 芬蘭 |
| `similar` | 3.2.1 | Apache-2.0 | wiki 差異 | Armin Ronacher（mitsuhiko） | 奧地利 |
| `unicode-normalization` | 0.1.26 | (MIT OR Apache-2.0) AND Unicode-3.0 | wiki 標題 NFC／NFKC | unicode-rs 團隊 | 【推論】美國為主，未查證 |

- 授權都和 GPL-2.0-or-later 相容（Apache-2.0 → 以 GPLv3 散布，D3）。`cargo deny` 的授權允許清單要加 `BSD-2-Clause`、`BSD-3-Clause`、`Unicode-3.0`、`Unlicense`（若還沒有）。
- 間接依賴（D17 只列不擋）：comrak 會帶 `typed-arena`、`entities`、`unicode_categories` 等；ammonia 會帶 `html5ever`、`markup5ever`、`tendril`（Servo 專案）。實際清單在 M2.0 加入後用 `cargo tree` 列出，寫進 decisions。
- NFKC_Casefold 的 case folding 表：`unicode-normalization` 只有正規化、沒有完整 case folding；來源（PostgreSQL 18 的 `casefold()`，或另一個 crate）在附錄 B C13 決定。若要新 crate，照 D17 另列、另外核可，本輪沒有選。
- **不需要**的：multipart（B-15）、圖片處理（不產縮圖）、掃毒、HTTP client（沿用 `github` feature 的 hyper）。`tower` 從 dev-dependency 移到正式依賴（已在 lockfile），開 `limit`、`load-shed` feature。

### 9.3 和 Redmine 的差異（補 02 §12、05 §9.3）

| # | 項目 | Redmine | oxledger M2 | 理由 |
|---|---|---|---|---|
| D-M2-1 | 議題參照語法 | `#123`、`r123`、`commit:abc` 等 | 只有 `KEY-123`、`@login`、`[[頁面]]` | D9；commit 參照由開發卡呈現 |
| D-M2-2 | 看不到的參照 | 【推論：Redmine 對看不到的 `#123` 輸出原文】 | 輸出原文，和不存在相同（明文保證） | 存在性 |
| D-M2-3 | HTML | CommonMark 模式允許部分 HTML 並消毒【推論】 | 一律跳脫 | 少一個攻擊面 |
| D-M2-4 | 修正關鍵字 | 以 commit 作者對應使用者執行【推論】；全站設定 | 以規則 sponsor 執行；每專案設定；API 確認在預設分支 | A3 |
| D-M2-5 | commit 記工時 | 有 | 沒有 | B-22 |
| D-M2-6 | 匯入 | 精靈、可設定、有「更新」以外的選項【推論】 | 逐列走命令引擎、試跑、撤銷；不更新既有單 | U31、B-27 |
| D-M2-7 | 匯出上限 | 預設 500【推論】 | 10,000 | B-31 |
| D-M2-8 | 附件去重 | 依檔案 digest、跨專案【推論】 | 專案內去重 | 研究 §5.5 |
| D-M2-9 | wiki 刪除 | 硬刪 | 軟刪、30 天可還原 | 稽核、undo |

### 9.4 本文以外要改的文件（這次沒改）

> M2.0（2026-11-13）已改：02 §19.1、R-G4、§19.2、§3.8、§16.7；05 §3.11。00、01 §14.8 與研究 §8 還沒改（不在 M2.0 範圍）。

- 00：A1–A6 的決定結果；U45b 的狀態（若 A3 照建議）。
- 01 §14.8：依 A1 改圖件的階段（建議改成 P5）。
- 02 §19.1、R-G4：「不寫 issue」改成「除了 `fixed_by` 規則（06 §3.8）」；§19.2 加訂 `push`。
- 02 §3.8：加註「wiki 讀取在 W(P) ⊇ A(P) 時不改變保證；wiki 目的地讀者 W(P)」（06 §3.12）。
- 02 §16.7：wiki 與附件的 undo（06 R-W9、R-A7）。
- 05 §3.11：通知原因加 `mentioned`。
- 研究 §8：M2 的進度補記（照 M0 的寫法）。

---

## 10. 風險

| # | 風險 | 影響 | 對策 |
|---|---|---|---|
| R1 | Markdown 消毒漏洞（comrak 或 ammonia 的 bug、我們的佔位節點處理錯） | 儲存型 XSS，可冒用使用者 session 寫入 | 兩層（escape＋ammonia）、CSP 無 inline script、`{@html}` lint、T-M2-01 語料；依賴升級時重跑 |
| R2 | 參照解析變成存在性 oracle（快取鍵、title、class 有差異） | 洩漏看不到的單是否存在 | 第二階段不快取；T-M2-02、04 以位元組比對 |
| R3 | `fixed_by` 被有合併權但不該改單的人利用 | 單被錯誤推進 | 預設關閉、限預設分支、API 確認、sponsor 權限、目標用非終態、可 undo |
| R4 | wiki 受眾與議題不一致造成 agent 外洩 | 違反 02 §3.8 | R-AG1／R-AG3 用集合包含；T-M2-41…43；E4w 重跑 |
| R5 | 附件被當成惡意檔案散布點 | 使用者下載惡意檔 | 非圖片一律下載、`nosniff`、`sandbox`；沒有掃毒（B-17，在升級說明明講） |
| R6 | 匯入大量資料拖慢系統、塞住 outbox | 搜尋延遲、通知延遲 | 每列一個短交易、5,000 列上限、預設不通知；監控 D11 frontier |
| R7 | 舊內容 `plain` 與新內容 `markdown` 混用造成困惑 | 使用者看到兩種顯示 | 編輯時提示；不做自動轉換 |
| R8 | GitHub push payload 形狀與預期不同；一次推超過 3 個 tag 或 5000 個分支不送事件（已查證） | commit 遺漏 | commits 上限 2048 已查證；M2.6 用真 App 手動驗；開發卡標「可能不完整」；升級說明寫明批次推送的限制 |
| R9 | 附件檔案與資料庫備份不同步 | 還原後附件缺檔 | 升級說明寫備份程序；下載缺檔時顯示「檔案遺失」而不是 500 |
| R11 | 撤除過的 commit 被重新推上來 | 被撤除的訊息與連結重新出現 | 持久墓碑＋`redacted_at` 不覆寫（R-C3、R-C7、T-M2-65a） |
| R12 | 上傳或匯出耗盡磁碟、記憶體 | 服務中斷 | R-A2、R-E6 的配額、串流與門檻；T-M2-59、T-M2-86a |
| R10 | 新依賴的維護狀況 | 安全修補延遲 | `cargo deny`／`cargo audit` 進 CI；comrak 是 GitLab、Redmine（commonmarker）都在用的實作 |

---

## 附錄 A · 對照研究與來源

| 主題 | 來源 | 用在 |
|---|---|---|
| Redmine 的 wiki 與 CommonMark | 研究 01 §2（`lib/redmine/wiki_formatting`；commonmarker 底層是 comrak）【事實】 | R-M1、B-10 |
| GitHub 修正關鍵字 | GitHub Docs「Linking a pull request to an issue」（close／closes／closed／fix／fixes／fixed／resolve／resolves／resolved）【事實，寫作時依既有知識，未重新抓頁面】 | R-F2 |
| GitHub push 事件 | GitHub Docs「Webhook events and payloads › push」【事實，審查已查證：`commits` 最多 2048 筆；一次推超過 3 個 tag 或 5000 個分支不產生事件】 | R-C3、R-C4 |
| CSV 公式注入 | OWASP「CSV Injection」【事實，未重新抓頁面】 | R-E4 |
| 外部圖片與資料外送 | 05 附錄 D V23；設計 01 §14.5 | R-M3、R-M4 |
| SVG 上傳 XSS | 研究 04 §2.3（Discourse、TypiCMS 的 CVE；OWASP File Upload Cheat Sheet）【事實】 | R-A3、R-A6 |
| 附件存法 | 研究 01 §5.5（sha256 內容定址、以專案為範圍去重、參照計數＋GC、預留掃毒掛點）【事實】 | §2.4、§3.6 |
| crate 版本與授權 | crates.io API（2026-11-12）【事實】 | §9.2 |

## 附錄 B · 實作前要確認的程式現況

| # | 確認什麼 | 為什麼 |
|---|---|---|
| C1 | SPA 實際的 CSP 與 `Referrer-Policy`（前端部署）；SvelteKit 的 inline 樣式與啟動 script 需要的 `style-src`／`script-src`（是否要 `'unsafe-inline'` 或 nonce／hash）；上傳前本機預覽用的 `blob:` 是否要放進 `img-src` | R-M12、§6.2 |
| C2 | ~~`principals.login` 是否不分大小寫唯一~~ **結案**：0001 是 `login text UNIQUE`（區分大小寫唯一）；R-MN1 已依此改成「先精確、不分大小寫只在命中一人時」 | R-MN1 |
| C3 | `issue_view.rs`、`notifications.rs` 讀說明與留言的所有路徑（加 `*_html` 與純文字摘要） | R-M10、R-RF7 |
| C4 | `search.rs` 的索引欄位取值點（改成純文字抽取）與 layout 版本號 | R-M11、§3.14 |
| C5 | `commands.rs` 的 `KINDS`、`perm.rs` 的 op 常數、`agents.rs` 的政策常數、`audience.rs` 的 `DestKind`：wiki 目的地要接進去 | §3.11、§3.12 |
| C6 | `github/jobs.rs` 的 `target`／`target_of` 能不能共用到 push；`client.rs` trait 要加 `compare`、`pull`（含 body）、`commit` | §3.7、§3.8 |
| C7 | `notify` consumer 判斷原因的地方：加 `mentioned`、加「匯入且 `notify = false` 就跳過」 | R-MN5、R-I11 |
| C8 | 02 §16.7 undo 的實作入口（`undo.rs`）。**已查現況**：`exec_info` 的 owner 只認 `("agent_direct", 有 on_behalf_of)` 與 `("human_direct", 有 reverts_execution_id)`（撤銷的撤銷），其他 origin 回 None；所以人自己的 wiki／附件寫入、integration、匯入的撤銷都要新路徑或改用一般命令（R-W9、R-F10、R-I12）。實作前確認新路徑放在 `undo.rs` 還是 wiki 模組 | R-W9、R-A7 |
| C9 | 0008 `command_executions` 與 `issue_events` provenance CHECK 的現行全文（聯集重建要逐字取） | §2.2 |
| C10 | `field_admin.rs`、`views.rs` 的欄位目錄能否直接給匯入對應與匯出欄位用（只列讀得到／寫得到的） | R-E2、R-I5 |
| C11 | `audience.rs` 的 `holders()` 處理 Non member、Anonymous、系統管理員的方式，`wiki_audience()` 要逐字沿用；`Vis::Any` 的 SQL 片段能否共用 | R-AG1 |
| C12 | comrak 能否用選項設定註腳與標題錨點的 id 前綴；不行就在 AST 後處理改 id 與 `href` | R-M1 |
| C13 | NFKC_Casefold 的實作來源：PostgreSQL 18 的 `casefold()`（要哪個 collation provider 才是完整 folding）或新 crate（D17 另列核可）；兩邊結果要一致，因為唯一索引在 DB | R-W2 |
| C14 | `on_behalf_of_id` 其餘出現處（R-F10 列的檔案）對 `origin = integration` 的行為 | R-F10 |

## 附錄 C · 獨立審查處理表

2026-11-12 獨立審查（subagent）的意見，逐條處理如下。嚴重度沿用審查的分級。

| # | 問題 | 嚴重度 | 處理 | 改了哪些章節 |
|---|---|---|---|---|
| H1 | W(P) 沿用 `audience.rs::holders()`：它要求 `issue_tracking` 模組、只算 `issues_visibility` all／default，議題模組關或 own 角色有 wiki 權限時 W(P) 被低估，wiki 目的地檢查會誤判放行 | 高 | 另定 `wiki_audience(P, perm)`：wiki 模組開且未封存、不論 `issues_visibility` 一律計入（`Vis::Any`），Non member／Anonymous／系統管理員照 `holders()`；H(P) 同理。own 角色升級時不自動授 wiki（B-50）。補測議題模組關 wiki 開、own 角色有 `view_wiki_pages` | §0 K3、§1.2、§2.2、§3.12 R-AG1、§7.4 T-M2-42b／42c、§8、§9.2 B-50、附錄 B C11 |
| H2 | 撤除 commit 沒有持久墓碑，同 sha 再 push 會重建 ref、覆寫已遮蔽的 message | 高 | 新表 `project_commit_revocations`（不依附 commit 列）；R-C3 對有墓碑的 (project, repo, sha) 不建 ref、R-F3 不建 fix link；`github_commits.redacted_at`，upsert 不覆寫 message；遮蔽過的列不清除。補測撤除後再 push 同 sha | §2.1、§2.5、§3.7 R-C3／R-C7／R-C8、§3.8 R-F3、§3.13、§7.6 T-M2-65／65a、§9.2 B-42、§10 R11 |
| M1 | Markdown 圖片解析「同一個容器」會讓說明引用私有留言的附件 | 中 | 說明只解析議題層附件；留言解析自己的附件＋議題層附件；wiki 解析該頁附件 | §3.2 R-M4、§7.1 T-M2-07a |
| M2 | 遮蔽留言或說明時，附件沒有跟著處理 | 中 | 遮蔽留言連帶遮蔽它的附件並清本體；遮蔽說明時對話框讓管理者勾選議題層附件（預設不勾） | §3.13、§7.2 T-M2-21a／21b、§9.2 B-45 |
| M3 | 遮蔽 blob 後的附件狀態與重新上傳行為沒定義 | 中 | `attachment_blobs` 改以世代為列（`id` PK、`present` 唯一）；遮蔽時同 blob 的附件逐列標 `redacted_at`；重新上傳照常成功、建新世代、回應相同 | §2.4、§3.6 R-A4／R-A12、§3.13、§7.5 T-M2-58、§9.2 B-46 |
| M4 | 「本站相對路徑」沒擋 `//evil`、`/\evil`；ammonia 屬性沒限制值 | 中 | 相對路徑只接受單一 `/` 開頭且第二字元不是 `/`、`\`；ammonia `attribute_filter`：`img src` 限 `^/attachments/<uuid>/content$`、`input type` 限 checkbox 並強制 disabled | §3.2 R-M2、§7.1 T-M2-01 |
| M5 | 提及 chip 的判斷集合（所有成員）和自動完成（可指派清單）不一致 | 中 | 改成可指派清單，預設採用 | §3.3 R-RF3、§6.2、§7.1 T-M2-03a、§9.2 B-8 |
| M6 | login 大小寫規則未確認 | 中 | 已查 0001：區分大小寫唯一；先精確比對，不分大小寫命中多人輸出原文；附錄 B C2 結案 | §3.4 R-MN1、§7.1 T-M2-03a、§9.2 B-39、附錄 B C2 |
| M7 | 只處理 `distinct = true` 的 commit，fast-forward 合併到預設分支時 `on_default_branch` 與修正關鍵字會漏 | 中 | 推到預設分支時不論 distinct 都處理 `on_default_branch` 與修正關鍵字；distinct 只用來省重複建 ref；修 T-M2-61 | §3.7 R-C3、§3.8 R-F1、§7.6 T-M2-61 |
| M8 | `fix:{id}`、`import:{id}:{row}` 不是 uuid；integration 分支寫 `operation_request_id IS NULL` 與 0008 的 NOT NULL 衝突；worker 寫 `project_setting_events` 沒有 operation request | 中 | 改用 UUIDv5 推導 operation_id；integration 分支 `operation_request_id IS NOT NULL`，caller 是 `commands::SYSTEM_PRINCIPAL`；worker 自動停用規則時同交易以系統 principal 建 `operation_requests`，同規則同 lock_version 只停一次 | §1.4、§2.2、§3.8 R-F5／R-F8、§3.10 R-I9、§7.6 T-M2-70b |
| M9 | 誤稱 0023 暫停 `role_permissions_epoch`；實際 0023 暫停的是 `enabled_modules_epoch`，權限授予刻意 bump 一次 | 中 | 更正引用；寫出「不 bump」與「照 0023 bump 一次」兩案代價；預設不 bump 並說明為何安全，列 B 類，升級測試斷言前提 | §0 K13、§1.2、§2.1、§7.9、§9.2 B-37 |
| M10 | 外部 agent 讀工時（05 A1(c)）後寫 wiki，例外延伸到 wiki | 中 | 升為 A 類 A6，正文依建議 (b) 寫：開 `read_own_time` 的委派寫 wiki 一律送審 | §3.11、§3.12 R-AG11、§4.3、§7.4 T-M2-47a、§9.1 A6、00 |
| M11 | 匯出沒有串流、位元組上限與並行限制 | 中 | 逐列串流、50 MB 上限（超過中斷並記 `truncated`）、每人同時一個 | §2.7、§3.9 R-E6、§5、§6.11、§7.7 T-M2-86a、§9.2 B-40、§10 R12 |
| M12 | 上傳資源保護不足 | 中 | 路由個別放寬 body 上限、邊收邊寫暫存檔並算 hash；每人每日位元組配額（`upload_daily_usage`）；磁碟低於門檻 507；匿名不能上傳；每人同時 3 個 | §2.4、§3.1、§3.6 R-A1／R-A2、§5、§6.2、§7.5 T-M2-59、§9.2 B-41、§10 R12 |
| M13 | ChangeSet 套用的 agent 內容、人改寫 agent 內容的連結規則未定義 | 中 | external_agent／builtin_ai 提案經 ChangeSet 套用仍算 agent 內容；人改寫時編輯器提示「含 agent 寫入的連結」；複製不追蹤列 B 類 | §3.1、§3.2 R-M3、§6.2、§7.1 T-M2-13、§9.2 B-38 |
| M14 | A5 的代價漏了「wiki 沒有通知」 | 中 | A5 代價補上；加選項 (c) direct＋活動頁／首頁顯示 agent 最近改的 wiki，並改為建議；原 A-④ 併入 | §3.11、§4.3、§6.7、§9.1 A5、00 |
| O1 | R-F6 會透露 sponsor 看不到某張單 | 中 | 先以 sponsor 身分檢查可見性，看不到回 `workflow_disallowed`（同文案） | §3.8 R-F5／R-F6、§6.8、§7.6 T-M2-70a、§9.2 B-47 |
| O2 | R-F3（只對符合規則的寫 pending）與 R-F6（`no_rule`）矛盾 | 中 | 擇一：沒有符合的啟用規則時由 R-F3 直接寫 `suggested`（`no_rule`），不呼叫 API | §3.8 R-F3／R-F6、§7.6 T-M2-68b、§9.2 B-48 |
| O3 | GitHub API 用量沒有上限 | 中 | 每次 push 只呼叫一次 compare `{default}...{after}`（新欄 `confirm_head`）、每個 PR 一次；每次最多 200 列 fix link | §2.6、§3.8 R-F3、§7.6 T-M2-71a、§9.2 B-43 |
| O4 | `wiki_index` consumer 若只消費 wiki 列會卡住 outbox GC，也沒有世代機制 | 中 | 沿用 0018 的 `search@` 世代命名（`wiki@<uuid>`）、state 與藍綠；像 `notify` 一樣消費全部列、推進到 frontier；migration 不建 cursor | §2.1、§2.3、§3.14、§9.2 B-49 |
| L1 | K3「每份文件只動一頁」與 `wiki_delete` reparent 矛盾 | 低 | K3 改寫；寫出 reparent 的鎖序與 undo | §0 K3、§2.3、§3.5 R-W5／R-W9／R-W10、§7.3 T-M2-34a |
| L2 | `wiki_move` 並行可能成環 | 低 | 搬移、建立、刪除、還原與帶 title／parent 的 `wiki_put` 先鎖 `wiki_settings` 列 | §3.5 R-W4／R-W10、§7.3 T-M2-34b |
| L3 | `wiki_settings` 沒有回填與新專案建立；唯一衝突的錯誤碼 | 低 | 0033 回填＋新專案 trigger；唯一索引衝突一律 `wiki_page_exists` | §2.1、§2.3、§3.5 R-W10、§7.3 T-M2-26b、§7.9 |
| L4 | `title_key` 用 lower()、標題可含 Cf、`wiki_redirects` 可指向別專案 | 低 | NFKC_Casefold；標題與附件檔名禁 Cc、Cf；`wiki_redirects` 用 (project_id, page_id) 複合 FK | §2.3、§2.4、§3.5 R-W2、§7.3 T-M2-26a、§9.2 B-51、附錄 B C13 |
| L5 | WikiVersion 遮蔽沒包含修改說明 | 低 | 連 `comment` 一起換掉 | §2.3、§3.13 |
| L6 | 已刪除頁的歷史讀取規則未定 | 低 | 新 R-W16 與 `/wiki/deleted*` 端點：只有 `manage_wiki` | §3.1、§3.5 R-W16、§6.5、§7.3 T-M2-34c、§9.2 B-55 |
| L7 | undo 建頁的語意未定 | 低 | 建頁的 undo＝軟刪除 | §3.5 R-W9、§7.3 T-M2-34a、§9.2 B-56 |
| L8 | `undo.rs` 現況只支援 agent_direct 與撤銷 | 低 | 附錄 B C8 寫明現況；人／integration／匯入的撤銷走新路徑或一般命令 | §3.5 R-W9、附錄 B C8 |
| L9 | `format` 省略＝markdown 會把舊用戶端改的 plain 說明默默轉格式 | 低 | 更新時省略＝保留原格式；agent 更新 plain 說明要明寫；寫進升級說明 | §2.1、§3.2 R-M9、§3.11、§5、§7.1 T-M2-14a、§9.2 B-53 |
| L10 | 註腳 id 在同一頁多個容器時撞名 | 低 | 標題錨點與註腳 id 加容器前綴 | §3.2 R-M1、§7.1 T-M2-14、§9.2 B-54、附錄 B C12 |
| L11 | CSP 沒考慮 SvelteKit inline style 與 `blob:` 預覽 | 低 | 寫進 R-M12 與附錄 B C1 | §3.2 R-M12、附錄 B C1 |
| L12 | `filter_hash` 用純 sha256 可被字典還原 | 低 | 改 HMAC（server secret） | §2.7、§3.9 R-E8、§7.7 T-M2-85、§9.2 B-44 |
| L13 | commit 的 `author.username` 未驗證；`html_url` 取自 payload | 低 | 標未驗證並另存 `sender_login`；網址由 full_name＋sha 在讀取時組 | §1.1、§2.5、§3.7 R-C3／R-C6、§4.1、§6.8、§7.6 T-M2-65b、§9.2 B-20 |
| L14 | R-C11 寫錯：關閉開放後既有連動的行為 | 低 | 更正：關閉只擋新連動，既有連動照收 | §3.7 R-C11、§9.2 B-52 |
| L15 | CSV 匯入工時的使用者候選用「可指派」不對 | 低 | 改成本專案能記工時的人 | §3.10 R-I6、§6.12、§7.8 T-M2-98、§9.2 B-57 |
| L16 | webhook 並行上限只用 concurrency limit 會排隊 | 低 | 加 `LoadShedLayer`，滿了立即 503 | §3.7 webhook 安全總表、§7.6 T-M2-78、§9.2 B-32 與新依賴說明 |
| L17 | 匿名可以匯出 | 低 | 匿名不能匯出（兩個 A4 選項都一樣） | §3.1、§3.9 R-E9、§6.11、§7.7 T-M2-86、§9.1 A4 |
| L18 | R-F10 沒盤點 `on_behalf_of_id` 的使用處 | 低 | 盤點並寫出各處對 integration 的行為；「最近寫過」要排除 integration；其餘列附錄 B C14 | §3.8 R-F10、§7.6 T-M2-74a、附錄 B C14 |
| L19 | §1.2／§2.1 的 0023 trigger 名稱錯；CHECK 重建清單漏 `issue_events_kind_check`、`project_setting_events_kind_check` | 低 | 更正 trigger 名稱；清單補上兩個 CHECK 與各自新值 | §1.2、§2.1、§2.4、§2.5、§2.6 |
| L20 | R-C4 的 2048 上限待查證 | 低 | 審查已查證，結案；補記「一次推超過 3 個 tag 或 5000 個分支不產生 push 事件」 | §2.1 升級說明、§3.7 R-C4、§7.6 T-M2-63、§10 R8、附錄 A |
| A-① | 01 §14.8 與研究 04 把心智圖、BPMN 圖件排在 M2，06 沒提 | 升 A 類 | 併進 A1（新增選項 (c) 另開 M2b）；§1.1 補「不做」列 | 開頭說明、§1.1、§9.1 A1、§9.4、00 |
| A-② | A3 沒區分「PR 合併」與「直接 push 到預設分支」 | 升 A 類 | A3 加觸發來源維度 (i)／(ii)，建議 (i)；代價補外部貢獻者；正文 R-F1 依 (i) 寫，新原因碼 `direct_push` | §1.1、§3.8 R-F1／R-F6／R-F11、§6.8、§7.6 T-M2-68a、§9.1 A3、00 |
| A-③ | A1(c) 延伸到 wiki | 升 A 類 | 新 A6，選項 (a)／(b)／(c)，建議 (b) | 同 M10 |
| A-④ | A5 補 wiki 沒有通知 | 併入 A5 | 同 M14 | 同 M14 |
