# 01 · oxledger：Redmine 改寫成 Rust 的研究與架構建議

- 日期：2026-11-06
- 狀態：研究稿，架構未定；§9 的問題等使用者決定
- 視覺版：[01-redmine-rust-rewrite.html](01-redmine-rust-rewrite.html)

## 0. 目標（使用者原話整理）

1. 原生支援 MCP
2. 介面可高度客製化，功能對照 Jira
3. 參考 ontology + BM25 的設計，讓 AI 透過 MCP 時能快速理解、找到紀錄

## 1. 已定決策

| # | 決策 | 日期 | 理由 |
|---|---|---|---|
| D1 | repo 放使用者個人帳號、public | 2026-11-06 | 架構取之於社群 |
| D2 | 沿用 GPL v2 | 2026-11-06 | 同上；也因此可以直接參考 Redmine 原始碼，不必 clean-room |
| D3 | SPDX 寫 `GPL-2.0-or-later`（原 Q1） | 2026-11-06 | 與 Redmine 檔頭一致，可以使用 rmcp（Apache-2.0）；依賴授權用 `cargo deny` 檢查 |
| D4 | 不做資料遷移、不做 Redmine REST API 相容，全新 API 與 schema（原 Q2） | 2026-11-06 | 使用者沒有現有 Redmine 服務。連帶：不需要 Textile、不需要讀舊 EAV／YAML 欄位；§2「最難搬」的 ④⑦ 移除，M2 改為整合階段 |
| D5 | 單一組織：一個部署只服務一個組織，不做多租戶、不加 `workspace_id`（原 Q8） | 2026-11-06 | 要多個組織就各自部署；RLS 粗邊界因此只剩 project 層 |
| D6 | 前端用 SvelteKit SPA（TypeScript），渲染 server 送來的 layout JSON；型別由 Rust 產生（schemars → JSON Schema／OpenAPI → TS）（原 Q3） | 2026-11-06 | board、Gantt、grid、拖拉元件成熟；避開 Ant Design、ECharts、Element 等中國來源元件 |
| D7 | 只支援 PostgreSQL，不做 SQLite 單機版（原 Q4） | 2026-11-06 | 設計依賴 partial index、JSONB、xid8 快照、RLS、logical replication；權限與事件順序只需在一種 DB 上驗證 |
| D8 | 只支援 PostgreSQL 18 以上，主鍵用內建 `uuidv7()`（原 Q13） | 2026-11-06 | 新專案不遷就舊環境；部署文件要寫明版本需求 |
| D9 | issue 編號用專案前綴 `AUTH-123`（原 Q7） | 2026-11-06 | 一看就知道專案，AI 引用不易混淆。連帶：`issue_keys(key, issue_id, is_current)` 別名表，issue 換專案時發新號、舊號轉址；每專案一個計數器（同交易遞增，不用 sequence 以免跳號問題影響可讀性）；`fixes AUTH-123` 語法解析；MCP resource 改為 `…://issue/{key}`，內部仍以 UUID 為準 |
| D10 | 可見範圍每個請求即時計算，不物化快取（原 Q10） | 2026-11-06 | 和 Redmine 一致、沒有快取失效風險；E6 若顯示瓶頸再重議，且快取必須同交易重算並帶版本號 |
| D11 | outbox 用 xid8 快照排序：寫入時存 `pg_current_xact_id()`，消費端只讀 `xid8 < pg_snapshot_xmin(pg_current_snapshot())` 的列；at-least-once + 冪等（原 Q11） | 2026-11-06 | 只用一般 SQL 輪詢，好理解好測；延遲受輪詢間隔與長交易影響（要監控長交易）。設計 03 實測補充：**已分配 xid 的未結束交易會擋住 frontier，跨 database 也會擋**，告警要看 `pg_stat_activity.backend_xid`；大量更新要分批 |
| D12 | 事件紀錄「遮蔽後留痕」：誤貼機密或個資刪除時，payload 換成 `[已遮蔽]`，另記誰、何時、為什麼遮蔽；同步清除 tantivy 索引與快取（原 Q12） | 2026-11-06 | 稽核鏈不斷；已送出的 webhook 收不回來，文件要說明。保留期限預設永久、管理員可設定——此項為建議，使用者尚未明確確認 |
| D13 | 專案名稱 **oxledger**（原 Q5）；README 寫「derived from Redmine (GPL-2.0-or-later)」，不用 Redmine logo；MCP resource scheme 用 `oxledger://` | 2026-11-06 | 查證：crates.io、npm、GitHub 帳號 `oxledger` 都沒人用；網路搜尋無同名產品；oxledger.com 已被他人註冊（2024-09-14），oxledger.dev、oxledger.org 查無註冊（rdap.org 404，未確認可購）；商標資料庫未查 |
| D14 | 自訂欄位用**型別化 EAV**：`issue_cf(issue_id, field_id, v_num/v_date/v_text/v_ref/v_bool)`，每個欄位一個 covering partial index `(field_id, v_x, issue_id) WHERE field_id = …`；**不加** `issues.cf_cache JSONB`；Filter AST 每個條件必帶型別條件、一律用 `EXISTS`（原 Q9） | 2026-11-06 | E9（[02 報告](02-e9-custom-fields.md)）：冷門欄位篩選 1–5 ms vs JSONB 100–300 ms；寫入 8,313 vs 2,257 tps；刪熱門欄位 18 秒 vs 3.5 分鐘。代價是空間 2.7 GB vs 0.9 GB，planner 會低估列數 |
| D15 | 中文檢索：索引維持 unigram + bigram（不換 ICU、lindera）；查詢端在 bigram 之外加權重 0.1–0.2 的 CJK 單字；別名展開做成資料（`concept_labels`），效果要用獨立查詢集的 E2 再量；E8（向量）排在別名之後 | 2026-11-06 | E1（[03 報告](03-e1-zh-retrieval.md)）：nDCG@10 基線 0.781 > ICU 0.732、lindera 0.746；加單字 0.810（CI [+0.007, +0.055]）；別名展開 0.857 只算上限（查詢集與別名同一人寫）。CC-CEDICT（CC BY-SA 4.0）與 GPL-2.0 不相容，只留在實驗 |
| D16 | 前端走 AI 原生：產品目標是**意圖驅動的任務工作台**，工程先做「工作台 + AI 側欄」的底座；對話介面只當回報者的簡化入口。第一版的 AI 來源是**外部 agent 透過 MCP 提出 ChangeSet，人在 SPA 審閱、核准、套用**；給一般使用者試用前再補最小內建入口。外部模型出口（Q2）與受限資料發布（Q4）維持關閉（原設計 01 的 Q1、Q6） | 2026-11-06 | 設計 [01-ai-native-ux](../design/01-ai-native-ux.md) §12–§13；和 GPT 討論 9 輪 |
| D17 | 「不用中國產品」的判定：看開發者或公司所在地；範圍是直接依賴、模型、詞典、字型、託管服務；間接依賴只列清單、不硬擋（原設計 01 的 Q13） | 2026-11-06 | 有可操作的判定標準，供 `cargo deny`、選模型與字型時使用 |
| D18 | 受限自訂欄位可以開放給內建角色（Non member、Anonymous）；**篩選與顯示用同一套規則**：呼叫者在該專案的實際角色（同 `db::role_ids_for`）包含 `field_roles` 之一。刻意不照抄 Redmine「篩選只看成員角色」 | 2026-11-06 | migration 0003；避免「看得到欄位卻篩不到」；`custom_fields_pg` 的 Triage 欄位驗證兩條路徑一致 |
| D19 | 第一切片設計的使用者決定：agent 可同時委派多個受眾一致專案，以「工作脈絡 + 曾授予專案集合 + 提案版本來源快照 + 受眾包含檢查」防止內容搬到更寬的受眾（U1）；接受和 Redmine 的刻意差異（U2）；分流用逐張提案 + 佇列勾選核准（U11）；搜尋採基本排名契約（Q-S1，含 U7）；自訂欄位不就地改型別、提供轉換工具（Q-S9）。其餘待決照建議預設 | 2026-11-07 | 設計 [00 待決總表](../design/00-open-decisions.md)、[02](../design/02-first-slice-contract.md)（GPT 22 輪）、[03](../design/03-search-consistency-explain.md) |

> D3 的背景：rmcp（官方 MCP Rust SDK）是 **Apache-2.0 單一授權**，sqlx 是 MIT OR Apache-2.0。FSF 的立場是 Apache-2.0 與 **GPL-2.0-only 不相容**，與 GPLv3 相容。Redmine 原始檔檔頭本來就是「version 2 or any later version」，所以建議 SPDX 用 `GPL-2.0-or-later`（和上游一致），散布的二進位檔實際以 GPLv3 條款成立。若堅持 `GPL-2.0-only`，就不能連結 rmcp。（非法律意見）

## 2. Redmine 現況（要搬什麼）

來源：`gh api` 讀 redmine/redmine master（7.0.3，Rails 8.1，GPL v2+）。

| 面向 | 規模／事實 | 檔案 |
|---|---|---|
| Model | 約 95 個 model；migration 332 個；現存約 59 張表（粗算） | `app/models`、`db/migrate` |
| 權限 | 82 個 permission、10 個 project module；role 全站定義，透過 Member 指派到專案；issue 可見性 all／default／own + tracker 限定；SQL 版與 Ruby 版各寫一份 | `lib/redmine/preparation.rb`、`access_control.rb`、`issue.rb` L137–200 |
| Workflow | tracker × role × old→new status；欄位層級 readonly／required（含自訂欄位）；多角色合併 | `workflow_permission.rb` |
| 自訂欄位 | 13 種格式；可掛在 10 類物件；EAV（`custom_values`，值一律字串） | `lib/redmine/field_format.rb` |
| 查詢 | 40 個 operator（含相對日期、歷史值 `ev`/`cf`）；存檔查詢、group_by、totals 都在 SQL 做 | `app/models/query.rb` L287–352 |
| 全文搜尋 | `acts_as_searchable` = SQL `LIKE`，**沒有全文索引**；7 種搜尋類型 | `lib/plugins/acts_as_searchable` |
| 擴充 | Plugin DSL + 61 個檔案有 `call_hook`；外掛大量 monkey patch → **Rust 版不可能相容 Ruby 外掛** | `lib/redmine/plugin.rb`、`hook.rb` |
| API | 37 個 `.api.rsb`（JSON+XML 同源）；API key / Basic；OAuth2 provider（doorkeeper，6.1 起已有）；7.0 新增 webhook（Issue/News/WikiPage） | `app/views/**/*.api.rsb` |
| 整合 | MailHandler 收信建單；6 種 SCM（呼叫外部 CLI）；commit 關鍵字關單記工時；LDAP、TOTP、PDF、CSV | `mail_handler.rb`、`lib/redmine/scm/adapters` |
| 文字 | 自改 RedCloth3 Textile；CommonMark 用 commonmarker（底層就是 Rust 的 comrak）；`#123`、`r123`、`[[Wiki]]` 等連結語法與 8 個 macro | `lib/redmine/wiki_formatting` |

**最難搬（依難度）**：① 權限／可見性 SQL 組合 ② 動態 Query 引擎（EAV join、歷史值） ③ Issue 生命週期（workflow × 欄位權限 × 多角色、父子彙總、排程連動、journal diff） ④ Textile 相容渲染 ⑤ MailHandler ⑥ SCM adapter ⑦ 舊資料遷移（EAV 字串、YAML 序列化欄位） ⑧ 外掛生態（只能重新設計）。

**第一版可砍**：CVS/Bazaar/Filesystem/Mercurial SCM、Boards、Documents、News、Calendar、PDF、Import 精靈、LDAP、OAuth provider、reactions、XML API、舊外掛相容；Textile 只做唯讀渲染。

## 3. 對照 Jira（功能取捨）

| 功能 | Redmine | Jira Cloud | Rust 版 |
|---|---|---|---|
| Issue 類型與階層 | tracker + parent（無層級語意） | Epic／標準／Subtask，Premium 可往上加層 | **v1** type 帶 `level`，父子規則可設定 |
| Workflow | 轉換矩陣 + 欄位唯讀／必填 | 圖形編輯、conditions/validators/post-functions | **v1** 矩陣 + 宣告式 JSON 條件／驗證；**v2** post-function 交給 automation |
| 欄位與畫面 | 依 tracker／專案啟用 | 4 層 scheme | **v1** 欄位定義 + layout JSON（§5） |
| 查詢 | GUI 篩選，無查詢語言 | JQL | **v1** Filter AST（GUI、REST、MCP 共用）；**v2** 文字查詢語言編譯成同一 AST |
| Board／backlog／sprint | 外掛 | 內建 | **v1** Kanban；**v2** sprint、backlog |
| Roadmap／Gantt | 內建 | Timeline／Plans | **v1** 唯讀 Gantt；**v2** 可拖拉 |
| Dashboard | 我的頁面可拖拉 | gadgets | **v1** JSON 定義、可拖拉 widget |
| Automation | 無（7.0 有 webhook） | 有，有次數上限 | **v2** 規則引擎，不設上限 |
| Service desk／SLA | 無 | JSM | **不做**（v2 頂多 email-to-issue） |
| Time tracking | 強項 | 弱、靠 app | **v1** 保留 |
| Wiki | 內建 | Confluence | **v1** Markdown wiki |
| SCM | 內建 repo browser | Development panel | **v1** 收 webhook，不做 browser |
| 通知 | 個人偏好 | notification scheme | **v1** 個人偏好 + 專案預設，不做 scheme |
| 權限 | role × permission + private | permission scheme + security level | **v1** RBAC + per-issue 可見等級 + 「為什麼看不到」檢視器 |
| API | REST；7.0 webhook | REST v3、webhook、Forge | **v1** OpenAPI REST + webhook |
| AI／MCP | 無 | Rovo MCP（Cloud 限定、耗 credits） | **v1** 內建 MCP server |
| 行動版 | RWD | 原生 app | **v1** RWD |

**Jira 的坑要避開**：scheme 間接層太多、自訂欄位膨脹拖垮效能、company/team-managed 兩套模型、層級寫死、automation 計次、一般成員覺得太重。
**切入點**：Jira Data Center 2029-03-28 EOL，需要自架的團隊會找替代品。

## 4. 現有 Rust 專案

沒有成熟、能直接採用的 Rust issue tracker。peisear（axum + Askama + SQLite，0.2.x）、beads_rust（AI agent 導向、local-first）可參考寫法。設計參考：OpenProject（GPL-3.0，Redmine fork 演化；參考設計可以，**若複製其程式碼，合併後的作品只能以 GPL-3.0 散布**）、YouTrack（extension points）、Linear（view 模型）。

## 5. 架構建議

### 5.1 總覽

```
            ┌──────── 瀏覽器 SPA（渲染 layout JSON）
            │
axum ─┬─ REST (OpenAPI, schemars)
      ├─ MCP  (rmcp, Streamable HTTP 掛同一 router；stdio 給本機)
      └─ Webhook in/out
         │
   Domain core（權限、workflow、Filter AST、ViewDef）── 單一實作，三個入口共用
         │
   PostgreSQL（權威資料 + edges + concept_labels + outbox）
         │ outbox（xid8 快照或 logical replication，§5.5）
   tantivy 索引（BM25F，可由 DB 重建）
   背景工作：Postgres SKIP LOCKED 佇列（索引、郵件、commit 解析）
```

原則：**權限、workflow、查詢只在 domain core 實作一次**，REST、MCP、UI 都走它。Redmine 把可見性寫成 SQL 版和 Ruby 版兩份，是容易出錯的地方；Rust 版只保留「編成 SQL / tantivy filter」的一份規則，加性質測試（E4）對照 Ruby 版。

### 5.2 高度客製化介面

- **一切都是 ViewDef，存 DB、用 JSON 描述**：`{scope: user|team|project|global, kind: list|board|gantt|calendar|dashboard, query: FilterAST, group_by, sort, columns, layout, version}`。解析順序：使用者覆寫 → 專案預設 → 全域範本。
- **表單也是資料**：`FormLayout {type_id, project_id?, sections[{fields, when: Condition}]}`（OpenProject form config + YouTrack 條件式欄位）。
- **Server-driven，但只傳宣告式資料**：權限與驗證永遠在 server；layout 只管呈現。
- **AI 和人改的是同一份定義**：MCP 的 `list_views`／`save_view`／`query(filter_ast)` 直接吃 ViewDef/FilterAST 的 JSON Schema；變更留版本可 diff，AI 改了什麼看得到。
- **2026-11-06 補（設計 01 §14、§16）**：ViewDef 的 kind 擴充到 mindmap、ontology graph、BPMN 圖件，加上能力宣告與 view session（`view.*`、`diagram.*` 工具，`oxledger://view/{id}/render.png`）。**可持久化、使用者可編輯的 ViewDef 不是第一切片的前置條件**，第一切片用固定工作台。
- **外掛 UI**：有限具名 slot（`issue.sidebar`、`project.tab`、`dashboard.widget`），iframe／Web Component 隔離；後端外掛可考慮 WASM（wasmtime）。
- **主題**：CSS custom properties 做 design tokens。
- **前端框架**（§9 Q3）：建議 Rust 後端 + SvelteKit 或 React SPA（board、Gantt、grid 元件成熟）；全 Rust 就選 Leptos，但要預留 JS interop。htmx 只適合 v1 原型。避開 Ant Design、ECharts、Element（中國來源）→ Radix/shadcn、Chart.js/uPlot/Vega-Lite。

### 5.3 原生 MCP

- **SDK**：rmcp 3.x（spec 2026-07-28），Streamable HTTP 是 Tower service，直接掛 axum。
- **Auth**：server 是 OAuth 2.1 resource server（RFC 9728 metadata、RFC 8707 audience），授權伺服器用外部 IdP；stdio 用 API key。**實際權限 = OAuth scope ∩ Redmine 角色**。
- **Tools**（讀取合併、寫入按動作拆，方便對應權限與確認）：

| tool | 用途 |
|---|---|
| `search` | 結構化過濾 + 可選 `q`；`fields`、`detail=id\|brief\|full`、`limit`、`cursor`、`budget_tokens`、`facets` |
| `get` | 單筆，`include=[journals:last_n, relations, children, changesets]` |
| `neighborhood` | issue 的鄰域摘要（300–600 token） |
| `resolve` | 名稱／別名 → id，順便回傳合法值（狀態、tracker、使用者） |
| `create_issue`／`update_issue` | 帶 `expected_lock_version`（沿用 Redmine 樂觀鎖）、`operation_id`（冪等） |
| `transition` | 只接受 workflow 允許的狀態；不合法就回傳可用清單 |
| `add_note`、`log_time`、`link`／`unlink` | |
| `list_views`／`save_view` | 操作 ViewDef（§5.2） |

- **2026-11-06 更新：第一切片的實際工具契約以設計 02、03 為準**（上表是早期構想）。重點：
  - **agent 不能直接寫入**：寫入只透過 `changeset_propose`／`changeset_revise`，人在 SPA「核准並套用」（D16）。`create_issue`、`update_issue`、`transition` 等成為 ChangeSet 裡的操作，同一個命令引擎也服務人直接操作（設計 02 §6、§8）。
  - **新增工具**：`describe`（欄位 schema 與「現在能做什麼」）、`get_many`（一次最多 10 張）、`changeset_*`、`operation_get`、`verify_citations`、`get_source`（設計 02 §8.8、§14；設計 03 §2–§3）。
  - **輸入**：直接接受 `AUTH-123`、專案 key、tracker 名稱；錯誤附封閉的 `next_actions`，讓 agent 能自己修正。
  - **搜尋**：不回 BM25 分數，改回 `rank` 和 `match`；`explain` 片段從 SQL 原文產生；可驗證的 `citation_id`；寫入回執帶 `consistent_after` token，搜尋可等待或回 `search_lagging`（設計 03）。
  - **Auth**：第一版不做 OAuth 2.1。人用本機密碼 + cookie session + CSRF；agent 用可撤銷、綁委派的 bearer 憑證；舊 `api_keys` 降為唯讀（設計 02 §3）。
- **Resources**：`oxledger://issue/{key}`（D9：舊 key 轉址到新 key）、`oxledger://project/{id}`、`oxledger://wiki/{project}/{page}`、`oxledger://version/{id}`、`oxledger://changeset/{repo}/{rev}`。
- **Prompts**：`/triage`、`/sprint-review`、`/release-notes`。
- **控制長度**：預設 brief（一行約 30 token）、limit 20、cursor、超過 budget 截斷、journals 只給最後 n 筆。
- read-only 模式與 toolset 開關（學 GitHub MCP）。

### 5.4 Ontology + BM25（與 mnemos 對齊）

和 `F:\work\mnemos\design\01-theory.md` 一致的部分：BM25F、概念當加權詞、SKOS `prefLabel/altLabel`、沿關係乘 γ、先過濾再排序、outbox。刻意的差異：outbox 順序不能沿用 LSN（Postgres 多寫者，見 §5.5）、權威儲存用 PostgreSQL（不是 redb）、工具不壓到 3 個、PPR／Hebbian 先不做。

- **Ontology 存法**：PostgreSQL 型別化關係表，不用 RDF/OWL 或圖資料庫（schema 固定、權限要同交易判斷、1–2 跳用 recursive CTE 就夠）。
  - `edges(src_type, src_id, rel, dst_type, dst_id, weight, source, created_at)`
  - 關係：Redmine 原生（relates、duplicates、blocks、precedes、copied_to、parent）＋新增（`fixed_by` commit、`mentions`、`about` → Concept、Concept 間 `is_a`/`part_of`）
  - `concept_labels(concept_id, label, kind=pref|alt|hidden, lang, status=active|staging)`：bug＝缺陷＝defect＝臭蟲；LLM 提的別名先進 staging。
- **BM25**：tantivy（MIT，純 Rust）。欄位權重起點：title 3.0、concepts 2.0、description 1.0、journals 0.5、commit 0.7。
  - **可見性在搜尋當下過濾**（project、tracker、is_private、author、assignee 存成 fast field，依「每角色 (mode, tracker 集合)、多角色 OR、含使用者群組 id」組 Boolean filter），不能事後裁切，否則分頁與 facets 會洩漏；top-k 再回 DB 複驗。private notes 是獨立子文件、自己過濾（§5.5）。
  - **中文**：不用 jieba（lindera 的 jieba 詞典也不用）。基線用 unigram + bigram 雙欄位；E1 已比較 ICU、lindera + CC-CEDICT，基線勝出（D15）；CKIP（GPL-3.0）只適合離線補概念標籤。
  - 不選 ParadeDB pg_search：它是獨立 Postgres 擴充、不連結，授權本身不衝突；但 AGPL 的網路義務會落到每個部署方身上，且中文 tokenizer 未確認；SQLite FTS5 中文不行；Postgres 原生 FTS 不是 BM25。
  - 向量第一版不做；E1 顯示召回不足再用 RRF 併入（候選：multilingual-e5、Arctic-embed、Granite、EmbeddingGemma；避開 BGE、Qwen/GTE）。
- **查詢流程**：解析 → 別名展開（ontology）→ 參數過濾＋可見性 → BM25F → 沿 duplicates/parent/blocks 擴 1 跳重排（L′ + γ·G′）→ brief 回傳。
- **例子**：「上個 sprint 和登入相關、還沒修好的 bug 有哪些、誰在處理」
  ```
  search(q="登入", tracker="bug", version="@previous_sprint", status_class="open",
         fields=["id","subject","status","assignee","blocked_by"], facets=["assignee"])
  ```
  bug → tracker:Bug；`@previous_sprint` 由 server 依 Version 日期解析；登入 → Concept:Authentication（login、SSO、OAuth、sign-in）；duplicates 合併、有 `fixed_by` 的標「已有修正提交」；回傳 10 行 brief + `facets.assignee`。一次呼叫答完。

### 5.5 資料庫設計（D4 之後可以自由重設計；已依 2026-11-06 獨立審查修正）

| Redmine 現況 | 問題 | 新設計 |
|---|---|---|
| 自訂欄位 EAV：`custom_values(customized_type, customized_id, custom_field_id, value TEXT)` | 值一律字串，排序、比較、加總都要轉型 | **型別化 EAV**：`issue_cf(issue_id, field_id INT, v_num, v_date, v_text, v_ref, v_bool)`，依型別建 partial index `(field_id, v_x)`，篩選用 `EXISTS` semijoin；改型別、刪欄位只動該欄位的列。`issues.cf_cache JSONB` 只當讀取用的反正規化副本；facet、group by 交給 tantivy。不加 JSONB 快取，facet、group by 交給 tantivy。已由 E9 定案（D14） |
| 父子任務 nested set | 搬移時要改寫同一棵樹（同 `root_id`）的 lft/rgt | `parent_id`；**彙總值（完成度、估時加總、起訖日）寫入時算好存在父節點**，同一交易沿祖先鏈往上更新、依 id 順序上鎖；搬移時用 recursive CTE 檢查是否成環。這和 Redmine `recalculate_attributes_for` 的做法一致，列表頁才能依彙總值排序 |
| `journals` + `journal_details`（字串） | 歷史值查詢要掃字串 | `issue_events(issue_id, seq, at, actor, actor_kind=user\|ai\|automation, kind, schema_ver, payload JSONB)`，追加為主；另建型別化投影 `issue_field_changes(issue_id, field_id, old, new, at)` 給「曾經是某值」查詢。要有 **redaction** 機制（個資刪除、誤貼機密），不是絕對不可改（Q12） |
| private notes（`view_private_notes`） | — | 私人留言是獨立的可見性單位：在 tantivy 是獨立子文件／欄位、自己的過濾；webhook 與通知依接收者過濾 |
| polymorphic association | 無法建立 FK | watchers、attachments 這類目標種類少的表用 **exclusive arc**（多個可為空的 FK + `CHECK (num_nonnulls(...) = 1)`）。`entities(id, kind)` 只給 ontology 的 `edges` 用，並以 `(id, kind)` 複合 FK 加 CHECK 擋住種類錯誤 |
| `roles.permissions` YAML | 不能用 SQL 查 | `role_permissions(role_id, permission)` 正規化 |
| 可見性 | Redmine 每個角色一組（all／default／own × tracker 集合），多角色 OR；default／own 要比對使用者**和所屬群組** | **每個請求先算「可見範圍」**：`[(project, mode, tracker_set)]` 加使用者的群組 id，再編成 SQL 與 tantivy filter（Redmine 也是每次算）。先不物化快取（Q10）；真要快取，要列全失效來源：群組成員、`inherit_members`、模組啟停、專案公開性／關閉／封存、使用者鎖定與 admin、角色權限、Non-member／Anonymous 內建角色 |
| — | — | **RLS 只做粗邊界**（workspace／project），細規則留在 domain core，避免第二份實作；要 `FORCE ROW LEVEL SECURITY`、應用程式不用 table owner 連線、每個交易 `SET LOCAL` |
| `workflows` STI | 語意混在一起 | 拆成 `workflow_transitions` 與 `field_rules`，或版本化 JSON 定義 |
| `enumerations` STI | 型別混用 | 各自一張小表，或併入 `field_defs` 的 list 選項 |
| `settings`、`queries` YAML | 不可查、不可驗證 | JSONB + JSON Schema 驗證；queries → `view_defs` |
| 附件以檔名存磁碟 | 重複檔案 | sha256 content-addressed，**以專案為範圍去重**（跨專案去重會洩漏「某檔案存在」）；參照計數 + GC；預留掃毒掛點 |

**outbox 順序（修正原「LSN」說法）**：mnemos 的 LSN 成立是因為 redb 單一寫者。Postgres 的 sequence 順序 ≠ commit 順序（seq=10 的交易可能比 seq=11 晚 commit，消費者會永遠跳過 10）。做法二選一（Q11）：只消費 `xid8 < pg_snapshot_xmin(pg_current_snapshot())` 的列，或走 logical replication。消費端一律 at-least-once + 冪等。wiki、membership 等非 issue 異動也進 outbox。

**搜尋與 DB 的落差**：tantivy 會落後 DB（例如 issue 剛搬進私有專案）。先在 tantivy 過濾，再對回傳的 top-k 回 DB 複驗可見性；facet 計數也只算可見資料（寫進 E4）。

**其他原則**
- **ID**：內部 UUIDv7。PostgreSQL 18 起才有內建 `uuidv7()`；更早版本在應用程式端用 `uuid` crate 產生（Q13）。對人顯示的編號另存（Q7）。
- **狀態分類**：`issue_statuses.category = todo|doing|done`。
- **樂觀鎖**：保留 `lock_version`；自訂欄位寫入用 patch 語意，不整份覆寫。
- **刪除**：issue 封存不硬刪。
- **時間**：時間點用 `timestamptz`；開始日、到期日用 `date`；「今天」「本週」依使用者時區解析。
- **i18n**：`field_defs`、狀態、tracker 名稱要能多語系。
- **索引重建**：tantivy 用別名藍綠切換，記錄快照位置，重建期間照常寫入。
- **備份**：PostgreSQL 與 object storage 要能還原到同一時間點。
- **大量匯入**：走 `COPY`、批次寫事件、延後建索引。
- **通知**：digest 與去重。

### 5.6 技術棧

| 層 | 選擇 | 授權 |
|---|---|---|
| Web | axum + tower | MIT |
| DB | PostgreSQL 18+ | PostgreSQL |
| 查詢層 | sqlx（選 MIT 那一側） | MIT OR Apache-2.0 |
| 搜尋 | tantivy | MIT |
| MCP | rmcp 3.9 | **Apache-2.0** → 需要 GPL-2.0-or-later |
| Schema | schemars（REST/MCP/前端共用） | MIT OR Apache-2.0 |
| 背景工作 | Postgres `FOR UPDATE SKIP LOCKED` 自建 | — |
| Markdown | comrak（Redmine 現在就間接用它） | BSD-2 |

避開：SeaORM（SeaQL 主要作者在香港，依使用者「不用中國產品」的原則列為避開）、ParadeDB（AGPL 網路義務轉嫁給部署方）、jieba、BGE/Qwen。

## 6. 風險

1. **權限語意不一致 = 資料外洩**：最高風險，靠 E4 對照測試。
2. ~~相容範圍~~：D4 已定不相容，風險解除；剩下的是權限語意要自己定清楚（不必再對齊 Ruby 版，但 E4 仍可用 Ruby 版當參考答案）。
3. ~~繁中檢索品質沒有數據~~：E1 已量（D15）；剩下別名展開的真實增益要靠 E2。
4. **授權相容**：所有依賴都要過 GPL-2.0-or-later 相容檢查（CI 跑 `cargo deny`）。
5. tantivy 在 Datadog 收購 Quickwit 後的維護狀況未確認。

## 7. 待驗證實驗

| # | 驗證什麼 |
|---|---|
| E1 | ✅ 2026-11-06：繁中 issue 查詢，bigram vs ICU vs lindera(CC-CEDICT) 的 nDCG@10／recall@20 → D15，[03 報告](03-e1-zh-retrieval.md) |
| E2 | 欄位權重與概念欄位消融；別名展開要用**另一人寫的獨立查詢集**量（E1 的 0.857 是上限） |
| E4w | E4 延伸到寫入與衍生內容：每個命令 × 身分的權限矩陣、ChangeSet 套用、摘要／通知／快取不洩漏（設計 02 §9.7） |
| I | 提示注入回歸：非法引用、越權、私有內容被搬到公開處（設計 02 §9.8） |
| E3 | 1 跳圖擴展對重複單、被擋單的召回增益 |
| E4 | 可見性性質測試：隨機角色／private／群組指派，對照 Ruby 版 Redmine 結果 |
| E5 | LLM 工具使用：20 個 PM 問題的正確率、往返數、token；「少工具多參數」vs「一動作一工具」。分外部 agent 與內建 AI 兩條線；以設計 02 §14、03 §2 的呼叫序列當基準（常見用法 1–4 次往返） |
| E6 | 10 萬筆 issue 時 tantivy 近即時延遲、寫入後立即讀到；範圍改依設計 03 §4.10（`consistent_after` token，取代原本的 `min_seq` 構想） |
| E7 | rmcp OAuth + Claude／Cursor 的 Streamable HTTP 實連 |
| E8 | 向量 RRF 值不值得（E1 之後） |
| E9 | ✅ 2026-11-06：自訂欄位存法基準，100 萬 issue × 1000 欄位（稀疏），型別化 EAV vs JSONB → D14，[02 報告](02-e9-custom-fields.md) |

## 8. 建議的階段

- **M0 骨架**：axum + Postgres + 權限核心 + Issue/Project/User/Tracker/Status/Workflow + 最小 tantivy 索引 + MCP `search/get/resolve`（唯讀）。先跑 E4、E1、E9。
  - **2026-11-06 進度**：骨架完成（未含 E1、E9）。ox-core（可見性 `Pred`、workflow）；PostgreSQL schema（migrations/0001_init.sql）；每請求可見範圍 → SQL 與 tantivy filter；tantivy 索引（索引存 unigram+bigram、查詢端兩字以上只用 bigram）跟 xid8 outbox；MCP `search`／`get`／`resolve`（HTTP bearer key 與 stdio）。E4 雛形：40 個隨機世界、240 次檢查，SQL／索引／記憶體／service 四者都與 Redmine 規則的獨立實作一致；故意放入 bug 時第一個世界就失敗。尚未做：OAuth、寫入工具、ontology 概念擴展、REST。
  - **2026-11-06 補**：E9、E1 完成並採用（D14、D15）；實驗程式與原始結果在 `experiments/e9`、`experiments/e1`。
- **2026-11-06 補：第一切片**（回報 + 找相關紀錄 + 單筆分流）的實作順序已定稿：
  - **後端契約**（設計 02 §10）：S0 前置修補 → S1 身分 → S2 SPA API 骨架 → S3 權限與 describe → S4 命令引擎 → S5 ChangeSet → S6 agent 提案路徑 → S6b 審閱便利 → S7 D12 遮蔽 → S8 發布門檻（越權、未核准套用、重複效果都要是 0）。
  - **搜尋**（設計 03 §7）：第 1–3 步 search explain、第 4–6 步寫入後一致性屬於第一切片；第 7–11 步（指標、藍綠重建、D14 策略選擇等）排在下一個迭代。
  - D14 自訂欄位、D15、內建角色（D18）已完成。
- **M1 可日用**：自訂欄位（型別化 EAV，D14；schema 草案見 02 報告）、Filter AST、查詢端 CJK 單字低權重（D15）、別名展開、ViewDef（list/board）、issue_events、time tracking、MCP 寫入工具、完整 BM25F 欄位。
- **M2 整合**：CommonMark（comrak）、Git webhook、`fixed_by`、wiki、CSV 匯入匯出。（D4：不做 Redmine 匯入器與 Textile）
- **M3**：Gantt、dashboard、automation、sprint、外掛 slot。

## 9. 待使用者決定

- ~~Q1 授權寫法~~：已定 `GPL-2.0-or-later`（D3）。
- ~~Q2 相容範圍~~：已定全新 API、不遷移（D4）。
- ~~Q3 前端~~：已定 SvelteKit SPA（D6）。
- ~~Q4 DB~~：已定只支援 PostgreSQL（D7）。
- ~~Q5 repo 名稱~~：已定 oxledger（D13）。以下為查證紀錄：
  - 2026-11-06 查證結果：redmine.org 沒有找到公開的商標／名稱使用政策；搜尋也沒找到「Redmine」的商標註冊紀錄（**未直接查 USPTO／EUIPO／WIPO 資料庫，未確認**）。前例：2011 年 Redmine 開發者的 fork 先叫 Bluemine，後改名 ChiliProject，沒有沿用 Redmine 名稱。開源慣例（NLnet 的 fork 與商標 Q&A 等）是 fork 預設改名，但可以據實寫「based on Redmine」（指名性使用）。
  - 建議：另取新名字；README 與 About 寫「A Rust reimplementation inspired by / derived from Redmine (GPL-2.0-or-later)」，不用 Redmine 的 logo。MCP resource scheme 也跟著新名字（取代 `redmine://`）。
  - 2026-11-06 候選撞名檢查（crates.io API、GitHub repo 名稱搜尋；**未查商標資料庫、網域、npm**）：

    | 候選 | 意涵 | crates.io | GitHub 同名 repo |
    |---|---|---|---|
    | **ferrotrack** | ferrum（鐵）+ track；Rust＝鐵鏽 | 空 | 4 個，皆 0★ |
    | **oxtrack** | oxide（Rust 圈慣用）+ track | 空 | 3 個，皆 0★ |
    | **oxledger** | oxide + ledger（紀錄帳本） | 空 | 0 |
    | **trackwright** | 像 shipwright，打造追蹤的工匠 | 空 | 5 個，最高 1★ |
    | ruddle | 英文古字「紅赭土」，呼應 Red | 空 | 41，`Ruddle` 帳號有 974★ 專案 |
    | rubrica | 拉丁文「紅赭土」，rubric 的字源 | 空 | 2730（義大利文「通訊錄」，太通用） |
    | 已被佔用 | hematite、ochre、ferrite、lodestone、taconite、cinnabar、siderite、ontrack、tracksmith（crates.io 已有） | | |
- ~~Q7 issue 編號~~：已定專案前綴（D9）。
- ~~Q8 多租戶~~：已定單一組織（D5）。
- ~~Q9 自訂欄位存法~~：已定型別化 EAV（D14）。
- ~~Q10 可見範圍~~：已定每請求計算（D10）。
- ~~Q11 outbox 順序~~：已定 xid8 快照（D11）。
- ~~Q12 事件紀錄~~：已定遮蔽後留痕（D12）；保留期限預設永久待確認。
- ~~Q13 PostgreSQL 版本~~：已定 18 以上（D8）。
- **Q6 審查**：照慣例要不要另派獨立 subagent 研究並審查這份（或送 GPT 第二意見）再定？

## 附錄 A · 研究過程紀錄

- 2026-11-06 平行派出三組研究（只讀，不 clone、不安裝）：
  1. Redmine 架構：`gh api` 讀 redmine/redmine master（tree 2606 路徑、model、migration、`lib/redmine/*`、`query.rb`、`issue.rb`、`field_format.rb`、`mail_handler.rb`、`Gemfile`）。
  2. Jira 對照與 UI 客製：WebSearch/WebFetch。Jira 欄有部分憑既有知識，標未確認。
  3. MCP + ontology + BM25：讀 mnemos design 01、03 與 Aura `aura/memory/search.py`、`ontology/index.py`，加網路來源。
- 主 session 補查：`gh api` 確認授權 —— rmcp workspace `license = "Apache-2.0"`（v3.9.0）、sqlx `MIT OR Apache-2.0`、tantivy／axum／leptos MIT。由此發現 GPL-2.0-only 與 rmcp 的相容問題（§1）。
- 使用者決定 D3（or-later）、D4（不遷移、不相容），並指出資料庫可以優化 → 新增資料庫一節。
- 獨立 subagent 審查（自行查證 redmine 原始碼與 PG 文件），主 session 抽查屬實：doorkeeper 在 6.1-stable Gemfile 已有；`issue.rb` `visible_condition` 確實每角色一組規則並比對群組 id。採納的硬傷：① 可見性不能用單一欄位快取 ② private notes 會從搜尋洩漏 ③ Postgres sequence ≠ commit 順序，outbox 會漏事件 ④ JSONB 撐不住上千欄位的範圍篩選與排序（GIN 不支援範圍、planner 沒統計、寫入放大）。另修正：RLS 只做粗邊界、父子彙總寫入時算、exclusive arc、PG18 才有 uuidv7、日期用 date、ParadeDB 與 OpenProject 的授權說法、§5.5 編號缺漏、M0 沒有搜尋引擎。
- 研究間的分歧：Jira 組把 SeaORM 列為可用元件，MCP 組依政策標為避開 → 採避開。
- 更正前提：Redmine 最新是 7.0.3（2026-06-30 發布 7.0），不是 6.x。

## 附錄 B · 來源

- 名稱查證：https://nlnet.nl/NGI0/bestpractices/Forking_and_Trademarks.pdf ；https://en.wikipedia.org/wiki/Redmine （ChiliProject fork 改名）；https://www.fosshub.com/resources/maintainers/trademarks/
- Redmine 原始碼：https://github.com/redmine/redmine （`lib/redmine/version.rb`、`doc/COPYING`、上列各檔）
- Redmine 7.0：https://www.redmine.org/news/161 ；Features：https://www.redmine.org/projects/redmine/wiki/Features
- Jira 階層：https://support.atlassian.com/jira-software-cloud/docs/configure-custom-hierarchy-levels-in-advanced-roadmaps/
- Jira DC EOL：https://www.atlassian.com/licensing/data-center-end-of-life
- Rovo MCP：https://developer.atlassian.com/cloud/rovo-mcp/
- Jira 效能抱怨：https://community.atlassian.com/forums/Jira-articles/quot-Why-Your-Jira-is-Slow-Understanding-Bloat-and-How-to-Fix-It/ba-p/3040249
- Linear views：https://linear.app/docs/custom-views
- YouTrack extension points：https://www.jetbrains.com/help/youtrack/devportal/apps-reference-extension-points.html
- OpenProject 欄位：https://www.openproject.org/docs/system-admin-guide/custom-fields
- GitLab work item types：https://docs.gitlab.com/user/work_items/configurable_work_item_types/
- rmcp：https://github.com/modelcontextprotocol/rust-sdk
- MCP 授權規格：https://modelcontextprotocol.io/specification/draft/basic/authorization
- GitHub MCP：https://github.com/github/github-mcp-server
- ParadeDB：https://github.com/paradedb/paradedb
- SQLite FTS5：https://www.sqlite.org/fts5.html
- lindera：https://github.com/lindera/lindera
- tantivy：https://quickwit.io/blog/tantivy-0.24
- 本機：`F:\work\mnemos\design\01-theory.md`、`03-architecture-v1.md`；`F:\Aura\aura\memory\search.py`
