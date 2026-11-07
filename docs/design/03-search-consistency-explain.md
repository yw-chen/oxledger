# 設計 03 · 搜尋 explain、寫入後的搜尋一致性、D14 效能尾巴

- 日期：2026-11-06
- 狀態：可以直接照著寫程式的設計稿。不含產品程式碼。§10「待使用者決定」的項目先依建議預設寫，相關段落標註**〔依建議預設，待確認〕**。
- 視覺版：[03-search-consistency-explain.html](03-search-consistency-explain.html)
- 前提文件：
  - [01-ai-native-ux.md](01-ai-native-ux.md)：§4.5 可解釋、§10 能力清單、§15.7 錯誤格式。
  - [../research/01-redmine-rust-rewrite.md](../research/01-redmine-rust-rewrite.md)：D10 可見範圍即時算、D11 xid8 outbox、§5.4 檢索、§5.5 private notes。
  - [../research/02-e9-custom-fields.md](../research/02-e9-custom-fields.md)：D14、§8 planner 估計。
  - [../research/03-e1-zh-retrieval.md](../research/03-e1-zh-retrieval.md)：D15。
- 實測程式與原始結果：[`experiments/sd/`](../../experiments/sd/README.md)。暫時資料庫 `oxledger_sd_bench` 量完已經刪除。
- 討論方式：和 GPT（gpt-6.1-sol，effort high，thread `oxledger-search-consistency-explain`）討論 4 輪，2026-11-07 對齊設計 02 後再複審 1 輪（第 5 輪），紀錄在附錄 A。
  - GPT 對 tantivy 與 PostgreSQL 行為的說法，一律用原始碼或實測核對過才採用。
  - 核對的方法在 §3.1、§4.7、§5 標出。
- 範圍外：寫入命令、ChangeSet、身分驗證、SPA API 的整體設計在[設計 02](02-first-slice-contract.md)（契約線）。本文用到這些介面的地方直接引用 02 的章節；2026-11-07 依 02 附錄 C.3 對齊（11 項），對照見 §8.1。

## 0. 一頁摘要

1. **三種保證分開講**，不要混在一起：
   - **引用可驗證**：這段文字現在確實在呼叫者讀得到的來源裡。
   - **搜尋追上寫入**：對指定的交易 X，這次搜尋用的 reader 已經完整套用所有 xid ≤ X、而且有登記在搜尋 pipeline 的事件。這不代表 commit 的先後順序，也不代表跨系統的快照。
   - **跨 PostgreSQL 和 tantivy 的同一時間點快照**：**不提供**。
2. **search explain**：
   - **來源**：命中證據由**目前的 SQL 原文、套上呼叫者權限後**重新匹配產生。
   - **tantivy 的角色**：只負責召回。
   - **實測後不採用的兩個 API**：
     - `SnippetGenerator`：在現在的 tokenizer 下片段是空的；長度上限算的是 byte；而且忽略 boost，會把低權重的單字一起亮起來。
     - `Explanation`：JSON 裡有包含隱藏文件的文件數 `N` 和 `n`。
   - **偏移單位**：tokenizer 內部用 byte offset，對外一律用 Unicode code point。
   - **引用格式**：opaque 的 `citation_id`，不簽章。用 `verify_citations` 批次驗證，`exact` 和 `reanchor` 兩種模式分開。
   - **搜尋結果不再回傳原始 BM25 `score`**，改回 `rank` 加 `match{fields, strength}`。
3. **寫入後一致性**：
   - **token 只放寫入交易的 xid8**：`c1.<epoch>.<xid8>`。
   - **reader 帶排他 frontier F**，條件 `X < F` 成立就算追上。
   - **等待**：最多 `wait_ms`。追不上時，MCP 預設回正常的工具結果 `outcome: "search_lagging"`，不回錯誤也不回舊結果。
   - **剛建立的單要引用，走 `get(citations)`，不經過搜尋。** agent 不能直接寫入：它提案、人在 SPA 核准，agent 再從 `changeset_get` 取得建立的 key 和 token（02 §6.9、§8.8）。
   - 實測：
     - **已分配 xid** 的未結束交易會擋住 frontier，**跨 database 也會擋**；沒有分配 xid 的唯讀快照交易不擋。prepared transaction 另外監控。
     - CREATE INDEX CONCURRENTLY 建索引 18 秒，期間不擋 frontier。
     - 單一 UPDATE 改 95 萬列花 35 秒，整段都擋住 frontier。
4. **D14 尾巴**（100 萬筆 issue、1,770 萬列 `issue_cf` 實測）：
   - **範圍條件的低估**：實際是 2.6–17 倍，比原先記的 3–5 倍大。
   - **統計修正不全面做**：提高 statistics target 無效。逐欄位的 CASE 運算式統計能把估計修準，但多條件查詢仍要 53 ms，而且每個欄位都要多一個物件。
   - **改用執行策略**：先做上限 10,001 的索引探測（0.5–11 ms），再從兩種形狀選一種：
     - 現行 EXISTS。
     - 每個 EXISTS 加 `OFFSET 0`，變成逐列探測。
   - **E9 的多欄位 + LIMIT 案例**：51.5 → 9.8 ms；另外兩個案例 307 → 6.6 ms、43 → 3.2 ms。
   - **稀有交集要留在現行形狀**：同一個查詢逐列探測要 732 ms，現行形狀只要 4.6 ms。
   - **改型別**：分批就地轉換 80 萬個值共 13.1 秒，最慢一批 0.43 秒；遷移期間欄位暫停使用。**要不要支援改型別交給使用者決定。**
5. **以 MCP agent 為第一使用者**：§2 列 5 個主要用法和完整的呼叫序列。多數 2 次往返完成，粗估 0.5k–2k token（還沒用實際的 tokenizer 量）。

## 1. 現況與前提

### 1.1 現況（2026-11-06，`crates/oxledger/src/search.rs`、`service.rs`、`main.rs`）

| 項目 | 現況 |
|---|---|
| 索引欄位 | 下面列出各欄位的設定 |
| tokenizer | 下面列出各類字元的處理 |
| 查詢 | 下面列出 term 權重和 filter 的組法 |
| service | tantivy 先取 500 筆候選，回 SQL 用同一個 `Pred` 加 SQL 才有的篩選複驗。不夠就擴到 2000、8000，到上限就標 `truncated`。沒有 `q` 時直接走 SQL |
| 同步 | 下面列出流程與現在的限制 |
| 重建 | `rebuild()` 先 `delete_all_documents`，再對每張 issue 插一筆 outbox 重播。**重建期間搜尋是空的** |
| 輸出 | `IssueBrief` 帶原始 BM25 `score` |

**索引欄位**

- `id`、`key`：STRING STORED。
- 可見性欄位（`project`、`tracker`、`private`、`author`、`assignee`）：STRING。
- `subject`、`description`：自訂 tokenizer、WithFreqsAndPositions、**不是 STORED**。

**tokenizer**

- CJK：unigram 加相鄰 bigram。
- 拉丁字：轉小寫。
- 用 `_` 或 `.` 連起來的識別碼：分段之外再整串出一個 token。
- `Token.position` 是 token 序號，unigram 和 bigram 交錯排。
- `offset_from` 和 `offset_to` 沒設，**都是 0**。

**查詢**

- 兩個字以上的 CJK 用 bigram，權重 1，另加權重 0.1 的 unigram（D15）。
- 識別碼整串權重 1，分段權重 0.2。
- 欄位加權：subject ×3、description ×1。
- 可見性 `Pred` 編成 `ConstScoreQuery(0)`，用 AND 接上。

**同步**

- 觸發器在同一個交易裡寫 `outbox(xid8 = pg_current_xact_id())`。
- 單一 tokio loop 每 500 ms 讀一次 `xid < pg_snapshot_xmin(pg_current_snapshot())` 而且 `(xid,id) > cursor` 的列，每批 500 列。
- 每批的處理：delete_term、add、commit、reload，再寫 cursor。
- 限制：frontier 只存在 SQL 條件裡，沒有記下來，所以呼叫者沒辦法知道「我的寫入進索引了沒」。

### 1.2 這份文件刻意不保證的事

| 不保證 | 原因 |
|---|---|
| 搜尋結果和 SQL 是同一個時間點 | tantivy 和 PostgreSQL 不在同一個 MVCC 快照裡。最後一律回 SQL 複驗，所以結果只會少、不會多出看不到的東西 |
| `consistent_after` 滿足時結果一定包含那張單 | 可能是 query 不匹配、SQL 篩選不符、權限改了，或後來又被刪除 |
| 排名和看不到的文件完全無關 | BM25 的 `N`、`n`、平均長度包含所有文件（§3.8 基本契約） |
| explain 能完整解釋 BM25 分數 | evidence 是「目前原文裡哪些查詢詞出現在哪裡」，不是分數拆解 |
| 引用證明「伺服器曾經回傳過這段」或「這段在歷史上存在過」 | 不簽章，也不存歷史；只證明它現在存在而且你讀得到 |

## 2. MCP 主要用法與呼叫序列

第一使用者是外部 agent（Claude Code 等透過 MCP 連進來）。下面 5 個用法決定了 §3、§4 的 API 形狀。設計目標有四個：

- 往返少。
- 回傳精簡（`explain: "brief"`、`fields` 欄位選擇）。
- 錯誤碼能讓 agent 自己修正。
- 引用可以驗證。

token 量是工具參數加回應的粗估，不含工具 schema 與最後的自然語言回答。中文大約每 1–1.5 個字算 1 token。

### 2.1 工具一覽（新增與修改）

| 工具 | 現況 | 這份設計 |
|---|---|---|
| `search` | 有 | 加 `explain`、`fields`、`consistent_after`、`wait_ms`、`on_lag`。拿掉 `score`，改回 `rank`、`match`。加 `complete`、`outcome`、`consistency`，以及每筆的 `evidence` |
| `get` | 有 | 加 `fields`（真的只讀這些欄位）、`citations`（替指定欄位產生 citation）。02 §8.8 另加 `include`（actions、recent_notes、recent_events），兩者並存；`get_many` 用同一組參數（§3.6） |
| `resolve` | 有 | 加 `kind: "custom_field"`，回型別、單位、選項、運算子、`definition_version` |
| `verify_citations` | 新 | 批次驗證，`mode: exact｜reanchor` |
| `get_source` | 新 | 以 citation 取上下文：驗證、讀取、回新的 citation 一次完成 |
| 寫入 | 02 | **agent 沒有直接寫入工具**，只能 `changeset_propose`／`changeset_revise`，由人在 SPA 核准並套用（02 §8.8）。寫入回執（02 §6.9）帶 `search.consistent_after`：人直接操作看 `/commands` 的回應，agent 看 `changeset_get` 裡最近一次 execution |

所有讀取工具在 **agent 憑證**呼叫時，呼叫者的讀取範圍就是 02 §3.5 的委派讀取範圍：**工作脈絡內所有有效委派專案的聯集**（§3.8；U1 已決定可同時委派多專案）。一次搜尋可能回多個專案的結果，03 不預設單一專案篩選。

### 2.2 用法 A：「找跟登入有關、還沒修好的 bug」

| 步驟 | 呼叫 | 回傳要點 |
|---|---|---|
| 1 | `search({q: "登入", tracker: "bug", status_category: "open", limit: 5, fields: ["key","subject","status"], explain: "brief"})` | 5 筆以內，每筆一段約 60 字的命中片段，「登入」標成 `primary`，附 `citation_id`；另有 `complete: true` |

- **往返**：1 次（`tracker` 本來就接受名稱或別名，伺服器內部會 resolve；名稱有歧義時回 `ambiguous_reference` 並附候選）。約 600–1,200 token。
- **反推的設計**：
  - `status_category: "open"` 現在就有，不需要再加陣列。
  - agent 用不到全文和分數。
  - 空結果時，agent 必須先看 `complete` 和 `outcome`，才能回答「沒有」。

### 2.3 用法 B：「AUTH-123 有沒有重複的？」

| 步驟 | 呼叫 | 回傳要點 |
|---|---|---|
| 1 | `get({key: "AUTH-123", fields: ["subject","project","tracker"]})` | 只回標題、專案、tracker，約 80 token |
| 2 | `search({q: "<標題>", project: "AUTH", tracker: "<同上>", limit: 6, fields: ["key","subject","status"], explain: "brief"})` | 6 筆，agent 自己排除 AUTH-123；已關閉的單也算候選 |
| 3（選用） | `get_source({citation_id, quote, context_chars: 300})` | 只對最可能的一筆讀上下文，同時完成驗證 |

- **往返**：2–3 次，約 900–2,200 token。
- **反推的設計**：
  - 工具只回「候選」，不自動判定重複。
  - `get` 的 `fields` 要真的下推到讀取層，不能讀完再刪。
  - `get_source` 成功後不必再 verify 一次。

### 2.4 用法 C：「Severity 是 High 而且估時超過 8 小時的單」

| 步驟 | 呼叫 | 回傳要點 |
|---|---|---|
| 1 | `resolve({kind: "custom_field", text: "Severity"})` 和 `resolve({kind: "custom_field", text: "估時"})`（可並行） | Severity：list，選項 High 的 id，`definition_version` 3。估時：float，`unit: "hour"`，`definition_version` 2 |
| 2 | `search({filter: {and: [{field: "Severity", definition_version: 3, op: "eq", value: "High"}, {field: "估時", definition_version: 2, op: "gt", value: 8}]}, limit: 10, fields: ["key","subject","status","cf:Severity","cf:估時"], explain: "none"})` | 純 SQL 查詢：不經 tantivy，`consistency.status: "not_applicable"`，`match: null`。每筆的 `matched_filters` 列出兩個欄位值，一眼就看得出為什麼符合 |

- **往返**：並行時 2 次，否則 3 次。約 700–1,600 token。這一輪 agent 已經知道欄位時只要 1 次。
- **反推的設計**：
  - Filter AST 沿用 D14 現有形狀（欄位名稱或 id），可以帶 `definition_version`。
  - 版本不符時回 `field_definition_changed`，agent 重新 resolve。
  - 單位由 resolve 提供；欄位存分鐘時，agent 要送 480。
  - 查詢太慢時回可修正的 `query_timeout`，不偷偷縮小範圍（§5.6）。

### 2.5 用法 D：「剛建的單，我要馬上引用」

agent 不能直接寫入（02 的核心不變式）。建立要經過提案和人的核准：

| 步驟 | 呼叫 | 回傳要點 |
|---|---|---|
| 1 | `changeset_propose({operation_id, proposal: {<建立 issue 的命令文件>}})` | 02 §8.4 的 compact 回應：`changeset_id`、`state: "proposed"`、`validation`、`review_url`、`next_actions`。agent 把 `review_url` 交給使用者，請他到 SPA 核准 |
| — | （人在 SPA 核准並套用，不經 MCP） | 02 §8.6 |
| 2 | `changeset_get({changeset_id})` | 最近一次 execution：`{status: "succeeded", effects: [{issue_id, key: "AUTH-124", lock_version: 0, changed: true}], search: {consistent_after: "c1.…"}}`（02 §6.9） |
| 3 | `get({key: "AUTH-124", fields: ["subject","description"], citations: ["subject","description"]})` | 從 primary 的新快照讀，附兩個 citation |

- **往返**：3 次（不含人核准的時間），約 700–1,600 token。**完全不需要搜尋。**
- 第 2 步依 ChangeSet 的 `state` 和最近一次 execution 判斷（狀態名稱以 02 §8.2、§8.3 為準）：
  - 還在等核准：回答使用者「提案還在等核准」，**不要重送提案、不要請人重新核准**；之後再查一次 `changeset_get` 即可。
  - 已取消、已過期、不能執行：照 `next_actions` 回報，不自己重建提案。
  - execution 不是 `succeeded`：照 02 §14.2 處理（例如衝突就 `refresh_and_revise`）。
  - **只有 execution `status: "succeeded"` 才進第 3 步**，才說「已建立」。
- 使用者另外要求「確認搜尋得到」時才加一步：`search({q: "<關鍵詞>", consistent_after: "c1.…", wait_ms: 1500})`。
  - 回 `search_lagging` 時，agent 照工具描述回答：「已建立 AUTH-124，也已經可以引用；搜尋索引還在同步，不影響建立結果。」
  - **不准重送提案，也不要請人重新核准。** 「已建立」只看 execution 的 `status`，不看搜尋。

### 2.6 用法 E：「找出登入問題，引用來源回答我」

| 步驟 | 呼叫 | 回傳要點 |
|---|---|---|
| 1 | `search({q: "登入 問題", project: "AUTH", limit: 3, fields: ["key","subject","status"], explain: "brief"})` | 3 筆，各帶一段片段與 citation |
| 2 | `verify_citations({items: [{citation_id, quote}, {citation_id, quote}], mode: "exact"})` | 每筆分別回 `valid`、`source_changed`、`not_found` |

- **往返**：2 次，約 700–1,600 token。
- 片段的語境不夠時，第 2 步改用 `get_source`。
- 回 `source_changed` 時，用 `get_source(mode: "reanchor")`：
  - 引文只出現一處：回 `moved` 和新的 citation，agent 要重讀上下文再決定能不能引用。
  - 出現多處：回 `ambiguous`。

### 2.7 從用法反推的通則

1. **`brief` 是預設的 explain**：
   - 每筆 1 段，約 60 個 code point，含 citation。
   - `full` 是每個命中欄位最多 2 段，整份回應有字元上限。
   - 超出上限就回 `evidence_truncated: true`。這和結果不完整無關。
2. **`fields` 一律可選**：
   - 一定會回的欄位：
     - `key`、`rank`。
     - `match`：純 SQL 查詢時是 null。
     - `evidence`：`explain: "none"` 或純 SQL 時是空陣列。
     - 完整性欄位。
   - 其他欄位只回要求的。
   - 不給 `fields` 時，回現行 `IssueBrief` 去掉 `score` 之後的所有欄位。
3. **錯誤要帶修正線索**。格式和 02 §4.3 相同：頂層有 `code`、`request_id`、`retryable`、`retry_after_ms`、`next_actions`（02 §14.2 的封閉列舉），各碼的對照見 §3.7、§4.11。`details` 裡例如：
   - `details.path` 指出錯在哪個參數。
   - `details.expected_type` 說明該填的型別。
   - `details.suggested_tool` 指出可以先呼叫哪個工具，例如 `resolve`。
4. **agent 不碰 offset**：只傳回 `citation_id` 和 `quote`。
5. **工具描述寫明三件事**：
   - `search_lagging` 不是寫入失敗。
   - `complete: false` 時不能回答「不存在」。
   - 引用只能用工具回的 `citation_id`。這是對 agent 的行為契約；因為 citation 不簽章，技術上沒辦法強制（§3.6）。

## 3. Search explain（縮小版）

### 3.1 決策與核對依據

| 候選 API | 核對方式 | 結果 | 採用 |
|---|---|---|---|
| `SnippetGenerator`（`tantivy-0.27.0/src/snippet/mod.rs`） | 讀原始碼加探針程式（`experiments/sd/tantivy-probe`），結果在 `results/tantivy-probe.txt` | 見下方 | 否 |
| `Query::explain` / `Weight::explain`（`Explanation`） | 同上 | 見下方 | 否，不對外，也不寫進一般 log |
| postings positions | 讀 `search.rs` | position 是展開後的 token 序號，不是原文位置；索引不存 offset | 否 |
| 改成 STORED 再從 tantivy 取原文 | — | 拿到的是索引當時的版本，仍然要 SQL 授權和版本比對；索引也會變大 | 否 |
| **自己算：spanned lexer 加 SQL 原文** | — | 不必重建索引；證據一定來自目前讀得到的原文 | **是** |

`SnippetGenerator` 的實測結果：

- 用現在的 tokenizer（offset 全是 0），fragment 是空字串，highlight 全是空範圍。
- 補上 byte offset 後，`set_max_num_chars(150)` 實際比的是 byte，中文片段只剩 50 字。
- 它收 term 時權重一律用 `1/(1+doc_freq)`，**忽略 BoostQuery**。結果「入」在「輸入」「載入」裡也被亮起來。

`Explanation` 的實測結果：

- 回傳的是一棵 JSON 樹，3 個 term 就有約 6 KB。
- 裡面有 `N, total number of docs` 和 `n, number of docs containing this term`，**這兩個數字包含呼叫者看不到的文件**。
- 對 20 筆做 explain 約 0.6 ms；同一份 2 萬筆的索引，搜尋本身只要 0.06 ms。

### 3.2 來源單位與版本

| 名詞 | 定義 |
|---|---|
| 來源單位（source） | 一段有自己權限的原文。第一版有兩種：`{kind: "issue_field", issue_id, field: "subject"｜"description"}`，以及 `{kind: "issue_note", issue_id, note_id}`（只由 `get` 的 recent_notes 產生，§3.6）。之後的擴充見下方 |
| `source_rev` | `sha256(raw UTF-8 原文)`，完整 32 bytes，對外用 base64url。只看這一個來源，所以改 description 不會讓 subject 的引用失效 |
| `search_rev` | 索引文件和 SQL 內容是否一致：`sha256(v1 ‖ len(subject) ‖ subject ‖ len(description) ‖ description)`，以 STORED 欄位存進 tantivy。**這是索引 schema 變更**，要隨 §4.8 的新 generation 一起上線。舊 generation 沒有這個欄位時，一律當作「不同」，走重新檢查 |

之後擴充的來源單位：

- `{kind: "custom_field", issue_id, field_id, definition_version}`。
- note 進全文索引（Q-S2）時沿用 `issue_note`，不另開 kind。

### 3.3 Spanned lexer

把 `search.rs` 的 `Lexer` 改成輸出 `Lexeme { term, kind, from, to }`，`from` 和 `to` 是原文的 UTF-8 byte offset。同一份實作給三個地方用：

- tantivy tokenizer：這時才補上 `offset_from`、`offset_to`。position 維持不變，所以**索引格式不變，不用重建**。
- `tokenize()` 和 `query_terms()`。
- evidence 匹配。

規則：

- 用 `char_indices()` 記位置。
- 小寫只改 `term`，不改原文座標。一個字元轉小寫後變成多個字元時，那個 token 仍然對應原字元的完整範圍。
- 識別碼整串的 token 範圍，是從第一段開頭到最後一段結尾（中間的 joiner 也算在內）。
- 每份文字只建一次 byte ↔ code point 對照表，對外輸出前一次轉好。

### 3.4 Evidence 演算法

**輸入**：

- `query_terms(q)`：每個 term 帶權重。
- 呼叫者已授權的來源原文。讀法是**分批延後載入**：
  1. 先依排名取一批通過 SQL 複驗的候選（每批 2×limit 筆）。
  2. 讀這批的原文，做下面第 4 步的比對。
  3. 被淘汰的話，就再補下一批，直到湊滿一頁或候選預算用完。
  - 事前無法知道哪些會被淘汰，所以**不能**承諾只讀最後一頁的全文。

**原文比對（第 4 步的淘汰）不受 `explain` 開關影響**。`explain: "none"` 只是不回片段和 citation，舊的索引 term 仍然不能把已經不匹配的 issue 帶進結果。

**步驟**：

1. **分類查詢**：
   - `q` 沒給、是空字串、或只有空白：純 SQL 查詢，沒有全文證據。
     - 回 `match: null`、`evidence: []`。
     - 另回篩選條件命中的欄位值 `matched_filters`，只列呼叫者有權讀的欄位。
   - `q` 有非空白字元，但 `query_terms(q)` 是空的（例如只有標點）：回 `invalid_query`，不能默默變成列出全部。
2. **對每個來源做 tokenize**，用 term 相等找出所有出現位置。每個位置記下 `(from, to, term, weight)`。
3. **算命中強度 `strength`**：
   - 只要有一個權重 1 的匹配，就是 `primary`。
   - 有匹配、但全部是低權重 term（CJK unigram 0.1、識別碼分段 0.2）時，就是 `fallback`。
   - 同一個 term 在查詢裡有多個權重時，`query_terms` 會保留最高的那個。例如查 `invalid_grant invalid`，`invalid` 是 primary。
4. **比對 `search_rev` 與原文**（還沒有 `search_rev` 的舊 generation，一律當成「不同」處理）：
   - 和 SQL 原文相同：`evidence_status: "current"`。
   - 不同，但原文仍然命中：`evidence_status: "rechecked"`。這代表**排名可能還是依舊內容算的**，只有證據是新的。
   - 不同，而且原文已經不命中：丟掉這個候選，讓 service 的候選擴充繼續找。
   - 不能把舊的 term 或片段回給呼叫者。
5. **選片段**，下一節詳述。
6. **由 SQL 原文重算 `match.fields`**：舊 subject 的 term 已經消失、description 仍命中時，只列 description。

### 3.5 高亮對齊與片段選擇

**高亮規則**：

| 情況 | 規則 | 例子 |
|---|---|---|
| CJK bigram 重疊 | 範圍取聯集，不重複輸出字元 | 查「登入失敗」：「登入」[0,2)、「入失」[1,3)、「失敗」[2,4)，聯集是 [0,4) |
| 欄位裡有 primary | **只亮 primary**，低權重 unigram 不亮 | 查「登入」時，「輸入」裡的「入」不亮 |
| 欄位裡只有 fallback | 亮，標 `strength: "fallback"`；UI 用淡色 | 查「付不了錢」：在「付款失敗，錢沒有扣到」亮「付」和「錢」，不宣稱原文有「付不了錢」 |
| 識別碼整串命中 | 亮整串（含 joiner） | `invalid_grant` |
| 只有識別碼分段命中 | 只亮那一段，標 fallback | 查 `invalid_grant`，原文 `invalid request` 只亮 `invalid` |
| 整串和分段都命中 | 整串優先，不疊兩層 | — |
| 識別碼超過 128 bytes | 照現行語意只有分段，不補整串 | — |

**片段選擇**：

1. 開窗的中心：
   - 有 primary 的來源，以 primary 匹配為中心。
   - 沒有 primary 時，以 fallback 匹配為中心。
   - 同分時依序比：不同 term 數、權重總和，最後取位置比較前面的。
   - 欄位之間：subject 優先於 description。
   - 同一個 query 對同一份原文，結果要可重現。

   窗口大小：
   - `brief`：目標 60 個 code point，**最多 1 段**，選含最多不同 primary term 的那段。
   - `full`：每個命中欄位最多 2 段，每段目標 160 個 code point。
2. 窗口邊界盡量對齊句號、換行、空白，並往外延伸到 grapheme 邊界，避免把 emoji 或組合字元切斷。citation 本身仍以確切的 code point 範圍為準。
3. **不連續的兩段不能拼成一段**，也不能共用一個 citation。
4. **整份回應的預算**：`brief` 約 `limit × 120` 個 code point，`full` 最多 8,000 個 code point。
   - 超過時省略次要片段，回 `evidence_truncated: true`。
   - 這不影響 `complete`，因為結果頁本身是完整的。
5. 只為**實際回傳的片段**發 citation，避免指到沒回傳內容的 citation。

### 3.6 API 與資料格式

**`search` 參數（新增的部分）**

```json
{
  "q": "登入",
  "explain": "brief",
  "fields": ["key", "subject", "status"],
  "consistent_after": "c1.AY8b…",
  "wait_ms": 1000,
  "on_lag": "return"
}
```

- `explain`：`none`、`brief`、`full`。
  - MCP 預設 `brief`。
  - SPA 列表預設 `brief`，展開時用 `full`。
- `fields`：可選。合法的欄位有：
  - 固定欄位：`key`、`subject`、`project`、`tracker`、`status`、`status_category`、`assignee`、`is_private`、`created_at`、`updated_at`、`lock_version`。
  - `priority`：只有 02 的 U3 採用時才合法〔依 02 建議預設，待確認〕；未採用時回 `invalid_argument`。
  - 自訂欄位：`cf:<名稱>`。
  - 不認得的欄位回 `invalid_argument`，`details.allowed` 列出合法值。
  - `lock_version` 是給 02 用法 3（分流）一次拿到提案前置條件需要的版本；它是 SQL 複驗時讀到的值，不是索引裡的值。
- 其餘的一致性參數見 §4.4。

**`search` 回應**

```json
{
  "outcome": "ok",
  "complete": true,
  "truncated": false,
  "consistency": {"status": "eventual"},
  "items": [
    {
      "key": "AUTH-123",
      "subject": "SSO 登入後跳回登入頁",
      "status": "處理中",
      "rank": 1,
      "match": {"fields": ["subject", "description"], "strength": "primary"},
      "evidence": [
        {
          "field": "description",
          "evidence_status": "current",
          "text": "從公司入口登入後又被導回登入頁",
          "highlights": [
            {"start": 5, "end": 7, "strength": "primary"},
            {"start": 12, "end": 14, "strength": "primary"}
          ],
          "citation_id": "cit1.eyJ2IjoxLC…"
        }
      ]
    }
  ],
  "next_cursor": null,
  "evidence_truncated": false
}
```

- `highlights` 的範圍相對於 `text`，單位是 code point，半開區間。
- `text` 本身就是 citation 指的那段原文，agent 引用時原封不動地放進 `quote`。
- 拿掉 `score`。要比較強弱，看 `rank` 和 `match.strength`。
- `rank` 是**跨頁的序號**：第 2 頁從 `limit+1` 開始。
- `next_cursor` 綁定「標準化後的 q、篩選、排序、generation、query_semantics_version」：
  - 有任何不符就回 `invalid_cursor`。
  - generation 已經退役就回 `cursor_expired`。
- 分頁**不保證**並行修改下有固定的結果快照。「不漏不重」只在資料固定時測試。

**`citation_id`**

格式是 `cit1.` 加上 base64url 編碼的 JSON（第一版不簽章）：

```json
{"v": 1, "src": {"kind": "issue_field", "issue_id": "019…", "field": "description"},
 "rev": "<source_rev base64url, 43 字元>", "start": 7, "end": 22}
```

`src.kind` 是封閉列舉，一個 kind 對應一個來源單位：

| `src.kind` | 其餘欄位 | 來源單位 | 誰會產生 |
|---|---|---|---|
| `issue_field` | `issue_id`、`field`（`subject`、`description`） | issue 的單一欄位 | `search` 的 evidence、`get(citations)` |
| `issue_note` | `issue_id`、`note_id` | 一則 note 的本文 | `get`／`get_many` 的 `include: ["recent_notes"]`（02 §8.8）。note 不在共用索引裡（§3.8），所以 `search` 不會產生這種 citation |

- 不認得的 kind 回 `invalid_citation`。之後加來源種類（例如自訂欄位的文字值）就加新的 kind，`v` 不變。
- `issue_note` 的授權：issue 讀得到、而且那則 note 讀得到；private note 對 agent 憑證一律讀不到（§3.8）。查詢**一定同時限制** `note.id = note_id AND note.issue_id = issue_id`：citation 不簽章，偽造的配對（可讀的 issue 加別張 issue 的 note_id）必須回 `not_found`，不能跨 issue 或跨專案取出 note。note 被刪或改成 private 後回 `not_found`，形狀和不存在相同。
- `start` 和 `end` 是在**完整來源**裡的 code point 範圍。
- 用 UUID，不用 `AUTH-123`：issue 被搬到別的專案時 key 會變，UUID 不會。顯示用的 key 另外回。
- 不簽章的理由：需求是「伺服端能驗證引用是真的」。這只要「目前原文在這個範圍是這段字，而且你讀得到」就成立。代價是：
  - 證明不了「伺服器曾經回傳過它」。
  - 「只能引用工具回的 citation」只能是對 agent 的行為契約。
  - 要升級時，`cit2` 再加 MAC，並帶 `kid` 支援金鑰輪替。

**`verify_citations`**

```json
{"items": [{"citation_id": "cit1.…", "quote": "從公司入口登入後又被導回登入頁"}], "mode": "exact"}
```

逐筆回傳：

```json
{"results": [{"status": "valid", "key": "AUTH-123", "field": "description",
              "quote": "從公司入口登入後又被導回登入頁", "next_actions": []},
             {"status": "source_changed", "next_actions": ["fix_input"],
              "details": {"suggested_tool": "get_source", "mode": "reanchor"}}]}
```

每筆都帶 `next_actions` 陣列（對照見 §3.7 的逐筆狀態表）。

驗證順序（每一筆）：

1. 解碼。格式錯誤就回 `invalid_citation`。
2. 用目前呼叫者的讀取範圍讀來源（同一個授權上下文，§3.8）；agent 憑證的讀取範圍是委派讀取範圍。不存在或讀不到，都回 `not_found`。
3. `rev` 相同時，取原文 `[start,end)`，和 `quote` 逐字比對。
   - 相同就回 `valid`。
   - 不同就回 `citation_mismatch`。
4. `rev` 不同時，依 `mode` 處理：
   - `exact`：回 `source_changed`，只有在授權通過之後才回這個狀態。
   - `reanchor`：在目前原文裡找 `quote`。
     - 只出現一處：回 `moved` 加新的 `citation_id`。
     - 出現多處：回 `ambiguous`。
     - 找不到：回 `source_changed`。
5. 不做 Unicode 正規化、換行正規化或模糊比對。

上限：

- 每批最多 50 筆。
- `quote` 最多 2,000 個 code point。

- `mode` 預設 `exact`。
- 整批失敗只有三種情況：
  - 外層或 items 的型別錯誤、超過批次上限。
  - 服務不可用（回 `search_unavailable`，**不轉成 `not_found`**）。
  - 認證或委派失效（`unauthenticated`、02 的 `delegation_unavailable`）：整批回錯誤，**不降成逐筆 `not_found`**，也不回部分結果。
- 個別 citation 解碼失敗、範圍不合法、quote 不符，都只在那一筆回報。

**`get_source`**

```json
{"citation_id": "cit1.…", "quote": "…", "selection": "range", "mode": "exact", "context_chars": 300}
```

參數：

| 參數 | 值 | 預設 |
|---|---|---|
| `selection` | `range`：`quote` 必須等於 citation 的完整範圍<br>`subquote`：在 citation 範圍內找 `quote`，只出現一處才產生精確的子範圍 citation，出現多處回 `ambiguous` | `range` |
| `mode` | `exact`、`reanchor`，管的是「版本變了怎麼辦」，和 `selection` 是兩回事 | `exact` |
| `context_chars` | 0–2,000 | 200 |

處理順序：

1. 先授權並驗證版本。
2. 回傳 `{status, quote, citation_id, context: {text, highlights}}`：
   - `citation_id`：指向選定的 quote。
   - `context`：只是前後文，給 agent 閱讀；**不會**變成新的可引用範圍。

**只在同一個來源單位內擴張**：description 不會擴張到 subject，private note 也不會擴張到其他 note。

**`get` 的 `fields`**

- 合法值：`key`、`subject`、`description`、`project`、`tracker`、`status`、`status_category`、`status_since`、`assignee`、`is_private`、`created_at`、`updated_at`、`last_activity_at`、`lock_version`、`cf:<名稱>`；`priority` 同 `search`（02 U3 採用時）。不認得的回 `invalid_argument` 加 `details.allowed`。
- `fields` 選的是 issue 本身的欄位；02 §8.8 的 `include`（`actions`、`recent_notes`、`recent_events`，加 `note_limit`、`event_limit`）選的是附帶的區塊。兩者並存、互不影響，例如 `fields: ["subject","lock_version"], include: ["actions"]`。
- `get_many` 接受 02 §8.8 列的 `fields`、`citations`、`include`、`note_limit`，每把 key 一筆獨立結果。02 的 `get_many` 沒列 `event_limit`；`include` 含 `recent_events` 時先用 `get` 的預設值（已列入回饋 02）。
- `citations` 必須是 `fields` 的子集合，而且只能是 `subject`、`description`；要 citation 卻沒選那個欄位，回 `invalid_argument`（`details.path: "citations"`）。這樣「不讀未要求的正文」和「產生 citation」不衝突。
- 不給 `fields` 時照 02 §8.8：回現行內容，不含大段正文與歷史。

**`get` 的 `citations`**

`get({key, fields: ["subject","description"], citations: ["description"]})` 的回應裡，每個指定欄位多一個**全文定位用的 `citation_id`**（`src.kind = issue_field`）。`include: ["recent_notes"]` 時，每則 note 附一個 `src.kind = issue_note` 的全文定位 citation，規則相同：

- 短於 2,000 code point 的欄位，可以直接引用全文。
- 要引用其中一段，用 `get_source(selection: "subquote")` 換成精確的 citation。
- 全文超過 2,000 code point 時，這個 citation 只能拿來選子引文，不能直接驗證。

**長度上限**（三個工具共用）：

- 可直接引用、可驗證的 quote 最多 2,000 個 code point。
- 空的 quote 一律拒絕。
- `citation_id` 的長度、範圍型別、上下界都要驗證。

### 3.7 錯誤（格式照 02 §4.3）

格式：

```json
{"code": "field_migrating", "request_id": "0199…", "retryable": true,
 "retry_after_ms": 30000, "next_actions": ["wait_and_retry"],
 "message": "欄位維護中", "details": {"field": "估時", "retry_after_ms": 30000}}
```

- 這是錯誤物件本身。REST 照 02 §4.3 包成 `{"error": <錯誤物件>}`；MCP 照 02 §4.6 用工具結果加 `isError` 回同一個物件。本文其他地方的錯誤範例都是這個物件，不含外層。
- `retry_after_ms` 在**頂層**（沒有建議時間就是 null）；`details` 裡可以保留一份，但程式只讀頂層。
- `next_actions` 是 02 §14.2 的封閉列舉：`fix_input`、`refresh_and_revise`、`retry_same_request`、`wait_and_retry`、`ask_human`、`stop`。列出的是可選路徑，不是要依序全做。
- 錯誤碼小寫 snake_case，沿用 01 §15.7。

| code | 情況 | retryable | `next_actions` | details |
|---|---|---|---|---|
| `invalid_query` | `q` 有非空白字元，但沒有任何 term（例如只有標點）；全空白的 `q` 當成沒有給 | false | `fix_input` | `suggestion` |
| `invalid_filter` | 篩選條件格式錯誤、運算子和型別不合 | false | `fix_input` | `path`、`expected_type`、`suggested_tool: "resolve"` |
| `ambiguous_reference` | 名稱對應到多個欄位、專案、tracker 或使用者 | false | `fix_input`、`ask_human` | `candidates`（只列呼叫者有權使用的） |
| `field_definition_changed` | 篩選帶的 `definition_version` 和目前不同 | false | `fix_input` | `field`、`current_version`、`suggested_tool: "resolve"` |
| `field_migrating` | 欄位正在改型別（§5.7） | true | `wait_and_retry` | `field` |
| `query_timeout` | 篩選查詢用完預算（§5.6） | false | `fix_input`、`ask_human` | `deadline_ms`、`suggestions` |
| `invalid_cursor` | cursor 對應的查詢、篩選、排序或 query-semantics 版本和這次不同 | false | `fix_input` | — |
| `cursor_expired` | cursor 需要的 generation 已經退役 | false | `fix_input` | `suggestion: "rerun_search"` |
| `invalid_argument` | `explain`、`fields`、`wait_ms` 等參數不合法 | false | `fix_input` | `path`、`allowed` |
| `invalid_citation` | citation 解不開、版本或 `src.kind` 不支援 | false | `fix_input` | `path` |
| `not_found` | 來源不存在或不能知道它存在，兩種情況回應形狀相同 | false | `fix_input`、`stop` | — |
| `search_unavailable` | 索引壞了或 worker 停了（不是一般的延遲） | true | `wait_and_retry` | — |
| `context_unavailable` | agent 的工作脈絡已停用：R 中任一專案的權限設定改了（epoch 不符），或脈絡已關閉（02 §3.8、§4.4） | false | `ask_human`、`stop` | `kind`：`quarantined`、`closed`。訊息不得暗示「撤銷某個專案就能解除」 |

`verify_citations`、`get_source` 的**逐筆狀態**不是錯誤，不用錯誤外殼；但為了讓 agent 用同一套修正邏輯，每筆結果也帶 `next_actions`：

| 逐筆狀態 | `next_actions` | agent 該怎麼做 |
|---|---|---|
| `valid` | `[]` | 可以引用 |
| `moved` | `fix_input` | 改用回傳的新 `citation_id`，先重讀上下文 |
| `source_changed` | `fix_input` | 用 `get_source(mode: "reanchor")`；`details.suggested_tool: "get_source"` |
| `citation_mismatch` | `fix_input` | `quote` 要用工具回的原文，不自己改寫 |
| `ambiguous` | `fix_input`、`ask_human` | 給更長的 `quote` 或改用 `selection: "subquote"`；`details.count` |
| `not_found`、`invalid_citation` | 同上表 | — |

**`source_unverifiable` 不是 03 讀取工具的錯誤碼**。它是 02 在 propose 驗證提案 evidence 時（02 §6.7 步驟 7）回的外層錯誤；03 建議 02 在它的 `details` 帶上本表的逐筆狀態（例如 `details.citation_status: "source_changed"`），兩邊只維護這一份對照（已列入回饋 02）。

`not_found` 只用來隱藏「存不存在、讀不讀得到」，**不用來蓋過服務故障**。

### 3.8 可見性、受限欄位與排名側通道

**內容不洩漏（第一版就做得到，必做）**：

- 先授權，再讀原文。證據只從授權後的原文產生。
- 不回 `Explanation`，不回原始 score，也不回「全索引命中幾筆」。
- 自訂欄位、private notes 的權限，都是「issue 讀得到」**而且**「那個子來源讀得到」。
- `verify_citations` 和 `get_source` 每次都重新授權。
- **「呼叫者的讀取範圍」的定義**：
  - 人（SPA session、舊 `api_keys`）：那個人目前的可見範圍（D10，即時算）。
  - **agent 憑證：02 §3.5 的委派讀取範圍**＝ `load_scope(委派者) ∩ {工作脈絡內目前有效的委派專案（聯集）} ∩ 非私有 issue ∩ 非私有 note ∩ visible = true 的自訂欄位`，每個專案都要是受眾一致專案（02 §3.8）。已撤銷或到期的專案讀不到，但仍留在脈絡的曾授予集合 R（影響寫入，不影響讀取）。
  - **每次請求的前置檢查**（`search`、`get`、`get_many`、`verify_citations`、`get_source`、`resolve`）：工作脈絡要是 active，而且 R 中每個專案的 `security_epoch` 都和脈絡記錄相符；否則整批回 `context_unavailable`，不只檢查單一委派（02 §3.8）。
  - `search`、`get`、`get_many`、`get(citations)`、`verify_citations`、`get_source`、`resolve` 全部用**同一個授權上下文**：issue 層級編成 `Pred`（SQL 和 tantivy 共用）；note 再套 note 的私有規則，自訂欄位再套 `visible`／角色規則，`resolve` 的候選與 `details.allowed` 只列範圍內的專案、tracker、欄位、使用者。否則 agent 可以拿一個猜出來的 citation 去 `verify_citations`，用 `valid`／`not_found` 的差別確認私有內容存在（02 附錄 C.3 第 8 項）。
  - `get` 的 `include` 區塊也在同一個上下文裡算：`recent_notes` 先套權限再取 `note_limit` 筆（看不到的 note 不佔名額，也不透露被省略幾筆）；`recent_events` 遮蔽私有 note、受限自訂欄位，以及事件裡內嵌的這兩類舊值（照 02 §7.4、§7.5 的對外序列化）；`actions` 只列 agent 能**提案**的動作，不列委派者自己能直接做的。
  - 委派被撤銷或過期：撤銷 commit 之後才開始的授權檢查必須失敗，回 `unauthenticated`、02 的 `delegation_unavailable` 或 `context_unavailable`，不回部分結果。帶 `wait_ms` 等待中的搜尋，在等完、實際讀取之前重新檢查一次委派。已經送出的回應無法收回，不在保證範圍內。
- 一般 log 不記片段和 citation 的完整內容。
- 有快取時，key 要包含授權脈絡（呼叫者的可見範圍）與 `source_rev`。即使命中快取，仍然要重新授權，因為 `source_rev` 沒變**不代表權限沒變**。
- 下面這些地方都只列呼叫者有權使用的自訂欄位，不能讓讀不到的欄位透過名稱、型別或候選被確認存在：
  - 欄位名稱解析。
  - `details.allowed`、`candidates`。
  - `matched_filters`。
- `fields` 下推的意思是：不讀未要求的正文和自訂欄位值。授權和識別需要的內部欄位，例如 `project_id`、`is_private`，仍然照讀。

**排名側通道**：

- `ConstScoreQuery` 只保證 filter 本身不加分。
- BM25 的 `N`、`n`、`avgdl` 仍然包含看不到的文件，所以看不到的文件會影響看得到的文件的分數和排序。
- 拿掉 `score` 能防止有人反覆量同一個數值，但不能根除這個側通道。
- 本文採**基本契約**：

> 不洩漏看不到的原文、片段、命中的 term 和來源識別；不保證排名和看不到的語料無關。

  〔已決定 2026-11-07，§10 Q-S1：基本契約〕

**嚴格契約（若使用者選擇）**需要：

- 在截斷候選之前就套用「可讀來源 allowlist」。
- 改用不依賴全域統計的評分，例如固定的 IDF，或「term 是否出現」加上原文的 local TF 重新排序。
- 要重新跑 E1。

**private notes 與受限自訂欄位**：

- 在嚴格契約決定之前，**不進共用的 BM25**。
- 它們的讀取和篩選照常用 SQL。
- 要放進索引時，做成獨立的子文件，各自帶過濾欄位：
  - note：`note_private`、所屬 issue 的可見性欄位。
  - 受限欄位：`field_id`、可讀專案。
- issue 的總分只聚合看得到的子文件。
- 欄位權限有變化時（`field_roles` 異動），要對受影響的 issue 發 outbox。

〔依建議預設，待確認，§10 Q-S2〕

### 3.9 測試清單

- **lexer**：
  - 新的 lexer 吐出的 term 序列和現在的 `tokenize()` 完全相同（以 E1 語料全文比對）。
  - 所有 offset 都落在 UTF-8 字元邊界上。
  - 原文按 offset 切出來的字串，正規化成小寫後等於 term。
- **CJK**：
  - 重疊 bigram 的聯集正確。
  - 查「登入」時不亮「輸入」的「入」。
  - 查「付不了錢」時以 fallback 亮「付」和「錢」。
- **識別碼**：
  - 整串、只有分段、超過 128 bytes、前後有 joiner、連續 joiner。
  - `invalid_grant invalid` 的 `invalid` 是 primary。
- **Unicode**：
  - emoji ZWJ、組合字元、CRLF、`İ` 這類小寫後會變長的字元。
  - code point offset 和 JS `Array.from` 結果一致（用固定案例對照）。
- **索引不同步**：
  - 索引是舊內容、SQL 已經刪掉那個 term：不回舊片段，候選被丟掉。
  - 只剩 fallback 命中：`strength` 降為 fallback。
- **權限**：
  - 撤銷權限、issue 改成 private 之後，`verify_citations` 回 `not_found`。
  - 「找不到」和「沒權限」的回應位元組相同（`request_id` 除外）。
  - agent 憑證（委派讀取範圍），對私有 issue 的欄位、私有 note、`visible = false` 的自訂欄位、委派專案以外的 issue，各工具分別驗：
    - `verify_citations`、`get_source`：手工構造的 citation 回逐筆 `not_found`，位元組和不存在的來源相同。
    - `get`、`get_many`：目標 issue 不在範圍內回 `not_found`；`include: ["recent_notes"]` 直接省略看不到的 note，結果和那則 note 不存在時相同；`recent_events` 不含被遮蔽的值。
    - `resolve`：範圍外的專案、欄位不出現在候選裡。
    - 同一個委派者用 SPA session 讀得到（確認測的是委派範圍，不是資料不存在）。
  - 委派撤銷 commit 之後發出的請求，整批回 `delegation_unavailable`（或 `unauthenticated`），不是逐筆 `not_found`；等待中的搜尋在讀取前重驗而失敗。
  - 多專案脈絡：同時委派 A、B 時，搜尋回兩個專案的結果；撤銷 B 後讀不到 B，但 A 照常；R 中任一專案改權限設定後，所有讀取工具整批回 `context_unavailable`。
  - `issue_note` citation：note 改成 private 或被刪除後回 `not_found`；改另一則 note 不影響這則的 `rev`；`issue_id` 配上別張 issue 的 `note_id` 回 `not_found`。
- **引用驗證**：
  - `source_rev` 只看單一欄位：改 description 不影響 subject 的 citation。
  - `exact` 和 `reanchor` 的差異；quote 出現多處時回 `ambiguous`；quote 和範圍不符時回 `citation_mismatch`。
- **預算**：
  - `brief` 在 `limit=50` 時回應大小有上限。
  - 超過 `full` 預算時有 `evidence_truncated`，而且沒有指向未回傳片段的 citation。
- **分數**：輸出不再有 `score`。

## 4. 寫入後的搜尋一致性

### 4.1 定義

| 名詞 | 定義 |
|---|---|
| H | 同一個 SQL statement 的快照 S 下的 `pg_snapshot_xmin(S)`。所有 xid < H 的交易，在 S 時都已經結束（commit 或 abort） |
| 內部 cursor C | `(last_xid, last_id)`，消費者讀到哪裡。同一個 xid 的事件跨批次時，靠它不漏、也不重播整個交易 |
| 排他 frontier F（`complete_before_xid`） | 「所有 xid < F 的交易，它們的搜尋事件都已經套用並發布到這個 reader」 |
| 一致性 token | `c1.<epoch>.<X>`，X 是寫入交易自己的 `pg_current_xact_id()` |
| 滿足條件 | 這次搜尋用的 `SearchView` 的 F 滿足 **X < F** |
| 索引延遲 | 從寫入交易 commit，到包含它的 `SearchView` 發布的時間（量法見 §4.6） |

**為什麼 token 只要 xid**（第 2 輪 Claude 提議，GPT 同意，取代它第 1 輪的 `(xid8, outbox_id)`）：

- X < H 表示 X 在快照 S 時已經結束。它寫的 outbox 列對 S 可見，所以會在這一批或更早被讀到並套用。
- 命令層不必找出自己寫了哪幾列 outbox；批次命令也自然涵蓋。
- **限制**：只保證「有登記到 outbox 的變更」。漏了觸發器的變更，frontier 再正確也救不了，所以要另外測 outbox 涵蓋率（§4.11）。

### 4.2 同步迴圈（取代 `SearchIndex::sync`）

由單一 task 擁有 `IndexWriter`，用型別保證同一時間只有一條寫入路徑。每一輪：

```sql
-- 同一個 statement：H 和可見的事件來自同一個快照；零筆事件時也拿得到 H
WITH h AS MATERIALIZED (SELECT pg_snapshot_xmin(pg_current_snapshot()) AS h),
     ev AS (
       SELECT o.id, o.xid, o.entity_id
       FROM outbox o, h
       WHERE o.topic = 'issue' AND o.xid < h.h AND (o.xid, o.id) > ($2::xid8, $3::bigint)
       ORDER BY o.xid, o.id
       LIMIT 500)
SELECT h.h::text AS h,
       coalesce(json_agg(json_build_object('id', ev.id, 'xid', ev.xid::text, 'entity', ev.entity_id)
                         ORDER BY ev.xid, ev.id) FILTER (WHERE ev.id IS NOT NULL), '[]'::json) AS events
FROM h LEFT JOIN ev ON true
GROUP BY h.h;
```

- `$2`、`$3` 是已提交的 cursor C。第一次從 commit payload 恢復，見下方。
- **`FILTER` 不能省**（第 4 輪 GPT 指出，PG 18.6 實測確認）：
  - 少了 `FILTER`，零事件時 `LEFT JOIN` 仍會產生一列，結果是 `[{"id": null, …}]`，不是 `[]`。
  - 加上 `FILTER` 後，零事件時回 `[]`；有事件時正常回傳每一筆。

1. 拿到 `H` 和 `events`。
2. 有事件時：
   - 讀這些 issue 的目前內容。這是另一個快照，可能比事件更新；同一張單之後的事件會再處理一次，所以沒關係。
   - 執行 `delete_term` 和 `add_document`（含 `search_rev`）。
3. 算出候選 frontier：
   - 事件少於 500 筆（已排空）：`F' = H`。
   - 剛好 500 筆：`F' = last.xid`。不能用 `last.xid + 1`，因為同一個 xid 可能還有第 501 筆。
4. 算出**暫定值** `F_cand = max(F_old, F')`，`C_cand` = 最後一筆事件。**不能覆寫已提交的 C 和 F**。
   - 這一輪任何一步失敗，下一輪都從已提交的 C 重來。
   - commit 結果不明時，重新讀索引的 metadata 確認，不能猜測成功，也不能跳過事件。
5. 執行 `prepare_commit()`，用 `set_payload(json)` 寫入 payload，再 `commit()`。commit 成功後，`C_cand` 和 `F_cand` 才成為 **durable** 狀態。payload 內容：
   - `{v, data_epoch, generation, cursor: {xid, id}, complete_before_xid: F_new, analyzer_version, query_semantics_version}`。
   - tantivy 0.27 有 `PreparedCommit::set_payload`，可用 `index.load_metas()?.payload` 讀回，探針程式實測可行。
6. 執行 `reader.reload()`，**建立並原子發布** `SearchView { searcher, generation, F, analyzer }`（`ArcSwap`），然後喚醒等待者。這時 F 才成為 **served** 狀態。
   - commit 成功、但 reload 失敗時，保留 durable 進度並重試發布。
   - 絕不能用舊的 searcher 搭配新的 F 發布。
7. 最後才更新 PostgreSQL 的 `outbox_cursors`。這張表只用來協調和監控，**不是恢復依據**。

**沒有事件、但 F 前進時**：

- 記憶體裡的 view 每輪都會更新 F，等待者可以通過，因為 searcher 沒變。
- durable payload 只在下面兩種情況才 commit，避免每 500 ms 產生一次空的 commit：
  - 有文件變動。
  - F 前進超過一段距離或一段時間（例如 10 秒）。
- 重啟後 F 從 payload 恢復，可能比重啟前保守。這只會讓等待者多等，不會錯。10 秒是持久化的目標間隔，不是故障時的恢復時間上限。

**重啟恢復**：

| 狀況 | 處理 |
|---|---|
| payload 有效，PG cursor 落後 | 以 payload 為準，PG cursor 改成和它一致 |
| PG cursor 領先 payload | **不能跳過**，以 payload 的 cursor 重播（at-least-once，重播無害） |
| payload 的 `data_epoch` 不符 | 不啟用這個 generation，觸發重建（§4.8） |
| payload 不存在或版本不支援 | 當成空索引，觸發重建 |
| reload 失敗 | 不發布新的 F，等待者不能通過；這一輪算失敗，下一輪重試 |

### 4.3 寫入回執（形狀以 02 §6.9 為準）

所有成功的寫入結果都帶同一個欄位 `search.consistent_after`（02 §6.9）。出現的地方：

- 人直接操作（`human_direct`）：SPA `/commands` 的回應。
- 人核准並套用 ChangeSet：approve-and-apply 的 execution。
- agent：`changeset_get` 裡最近一次 execution（agent 沒有直接寫入工具）；`operation_get` 也回同一個結果。

```json
{
  "status": "succeeded",
  "effects": [ { "issue_id": "0199…0701", "key": "AUTH-124", "lock_version": 0, "changed": true } ],
  "search": { "consistent_after": "c1.AY8bXk3q….1234567" }
}
```

- 新建立的 issue `lock_version` 是 0（02 §6.9）。
- 一份 ChangeSet 是一個交易，所以一份 execution 只有一個 token，放在 execution 層，不放在每個 effect。
- 全部 no_effect 時一樣回 token；零事件的交易也能滿足（§4.11）。
- token 在交易裡用 `pg_current_xact_id()` 取得，**commit 成功之後才放進回執**。
- **epoch 和 xid 一起持久化**：寫入端在同一個交易裡讀 `search_settings.data_epoch`，把**完整 token**（epoch 和 xid 的配對）存進結果紀錄（02 的 `changeset_executions.consistent_after`、`operation_requests` 的結果參照）。之後的 `changeset_get`、`operation_get`、冪等重播都回**原本那個 token**。**不能只存 xid、事後用當下的 epoch 重組**：epoch 輪替後重組出的 `c1.<新 epoch>.<X>` 會讓舊 xid 被當成新世界的 xid，繞過 `consistency_epoch_changed`。epoch 輪替後回原 token，由搜尋端回 `consistency_epoch_changed`。
- 格式是 `c1.<data_epoch>.<xid8 十進位>`：
  - `data_epoch` 是一個 128-bit 的 uuid，存在 `search_settings` 表，用 base64url 編碼。
  - 把資料庫復原成另一個分支、複製成獨立環境、或重新初始化搜尋資料時，**必須輪替 epoch**（寫進維運手冊，不靠 system_identifier 自動判斷）。
  - **輪替的協調點是 `search_settings` 那一列**（02 C.4 #12–#13）。02 的寫入交易讀 epoch 時對該列加 `FOR SHARE`。輪替用單列短交易 `UPDATE search_settings SET data_epoch = …`，並設 `lock_timeout`，所以會自然等進行中的寫入結束。**等這個交易提交之後**，才用新 epoch 切換索引、開始接受新 token。正確性不靠「停止寫入」；停寫入只保留為維運選項。搜尋端讀 epoch 不需要拿鎖。
- 不簽章。偽造較大的 X，最多只是讓自己的搜尋多等 ≤2 秒，不給任何讀取權。所以：
  - 每個呼叫者的等待並行數有上限（4）。
  - 全域的等待者也有上限（1,000），超過就立刻回 `search_lagging`。
  - 所有等待者共用同一個通知（`tokio::sync::watch`），不各自輪詢。
- **token 不能證明 commit**。「已建立」只能來自寫入回執的 `status`，搜尋服務不從 token 推論任何寫入狀態。

### 4.4 搜尋 API 的一致性參數與回應

| 參數 | 值 | 預設 |
|---|---|---|
| `consistent_after` | 一致性 token | 無（eventual） |
| `wait_ms` | 0–2000 | 1000 |
| `on_lag` | `return`、`error` | MCP：`return`；REST：`return` 〔依建議預設，待確認，§10 Q-S4〕 |

**分流規則**：

| 查詢類型 | 一致性行為 |
|---|---|
| 有 `q`（走 tantivy） | 等到 `SearchView.F > X` 或逾時。一拿到滿足條件的 view 就**固定用它**完成這次搜尋，不能中途換另一個 reader |
| 沒有 `q`（純 SQL） | 有帶 token 時，仍先驗證格式和 epoch。然後不等 tantivy，回 `consistency: {status: "not_applicable", reason: "sql_only"}`。SQL 在 primary、commit 之後建立的新快照執行，本來就讀得到自己的寫入。`not_applicable` 不代表任意 token 的交易已經 commit |
| token 的 epoch 不符 | 回錯誤 `consistency_epoch_changed`（retryable: false） |

**逾時（`on_lag: "return"`）**：

```json
{
  "outcome": "search_lagging",
  "complete": false,
  "consistency": {"status": "lagging", "waited_ms": 1000, "retry_after_ms": 500},
  "items": [],
  "next_cursor": null
}
```

- **不回舊的結果**，也不回分頁 cursor，免得 agent 把舊結果當成完整結果。
- 工具描述寫明：

> `search_lagging` 不是寫入失敗。不要重送提案，也不要請人重新核准；要引用剛寫入的內容，請用 `get`。

- `on_lag: "error"` 時改回錯誤 `search_not_caught_up`（retryable: true，頂層 `retry_after_ms`，`next_actions: ["wait_and_retry"]`）。

**正常回應的一致性狀態**：

- 有 token 而且滿足：`consistency.status = "satisfied"`。
- 沒有 token：`"eventual"`。

**`complete`、`truncated`、`evidence_truncated` 的差別**：

| 欄位 | 意義 |
|---|---|
| `outcome: "search_lagging"` | 指定的 token 還沒滿足 |
| `complete: false` | 這一頁不能當成完整的一頁：lagging，或全文候選擴充到 8000 筆還不夠 |
| `truncated: true` | 全文候選擴充到 8000 筆還不夠（現行語意不變）。這時 `complete` 一定是 false |
| SQL 時間預算用完 | **不是**不完整頁，一律回錯誤 `query_timeout`（§5.4） |
| `next_cursor` | 只有真的能接續時才給。lagging 時一律是 null；有回部分 items 也不代表有 cursor |
| `evidence_truncated: true` | 只省略了部分證據，和結果完不完整無關 |

### 4.5 SPA 的呈現

見 §6 的方便性檢查表。核心規則：

- 建立成功的提示來自寫入回執，搜尋同步中**另外**顯示。
- 列表上方可以放一張「剛建立」卡片，但不冒充搜尋命中。

### 4.6 索引延遲的定義與指標

| 時間點 | 來源 |
|---|---|
| `t_tx_start` | `outbox.created_at`（`now()`，交易開始時間，**不是 commit 時間**） |
| `t_commit` | `pg_xact_commit_timestamp(xid)`，需要 `track_commit_timestamp = on`。這台目前是 off，要重啟才能開，正式環境預設不開 |
| `t_commit_ack` | 寫入命令收到 COMMIT 成功的時間（應用程式內） |
| `t_published` | `SearchView` 發布的時間 |

| 指標 | 型別 | 說明 |
|---|---|---|
| `search_index_lag_seconds{bound="upper"}` | histogram | `t_published − t_tx_start`，對每批的事件算。**這是保守上界**（包含交易本身的時間） |
| `search_index_lag_seconds{bound="commit"}` | histogram | `t_published − t_commit`。只在開了 `track_commit_timestamp` 時有值（E6 用）。xid8 轉成 xid 時要注意 epoch，太舊的 xid 查不到就記 unknown |
| `search_consistency_wait_seconds` | histogram | 帶 token 的搜尋實際等了多久 |
| `search_consistency_outcome_total{outcome}` | counter | satisfied、lagging、epoch_changed |
| `search_outbox_pending` | gauge | cursor 之後的事件數 |
| `search_outbox_oldest_pending_age_seconds` | gauge | 最老的未套用事件 `now() − created_at`（交易時間的代理指標） |
| `search_frontier_stalled_seconds` | gauge | 「有待處理事件，或有等待者」時 F 停住多久。沒有新交易時 F 不動是正常的，不算 |
| `search_sync_last_success_timestamp_seconds` | gauge | 同步上次成功的時間 |
| `pg_oldest_xid_holder_age_seconds` | gauge | 見 §4.7 |

### 4.7 長交易：實測與告警

實測結果（`experiments/sd/frontier.sh`、`cic_frontier.sh`，PG 18.6）：

| 開啟中的交易 | 會擋 frontier 嗎 |
|---|---|
| 別的 database，唯讀 REPEATABLE READ（有快照、沒有 xid） | 不會 |
| 別的 database，已分配 xid（`pg_current_xact_id()`） | **會** |
| 同一個 database，唯讀 | 不會 |
| 同一個 database，已分配 xid | **會** |
| `CREATE INDEX CONCURRENTLY` 建 1,770 萬列索引（18 秒） | 不會（整段期間 `backend_xid` 都是空的） |
| 單一 `UPDATE` 改 95 萬列（35.3 秒） | **會，擋 35 秒** |

所以要監控的是「**整個 instance 裡已分配 xid 的最老交易**」，不是只看 `backend_xmin`（那主要擋的是 vacuum）。另外 `max_prepared_transactions` 目前是 0，開啟時也要看 `pg_prepared_xacts`。

```sql
-- 每 10 秒，監控帳號要能看到所有 database 的 pg_stat_activity（pg_read_all_stats）
SELECT datname, pid, usename, application_name, state, backend_xid,
       now() - xact_start AS xact_age, left(query, 120) AS query
FROM pg_stat_activity
WHERE backend_xid IS NOT NULL
ORDER BY xact_start
LIMIT 5;
SELECT gid, prepared, owner, database FROM pg_prepared_xacts ORDER BY prepared;
```

**初始告警閾值**（之後依 E6 和產品 SLO 調整）：

| 條件 | 等級 |
|---|---|
| 已分配 xid 的交易 `xact_age` > 15 s | warning |
| 同上 > 60 s | critical，附上 pid、應用程式名稱、查詢 |
| `search_frontier_stalled_seconds` > 10 | warning |
| `search_outbox_oldest_pending_age_seconds` > 5 s | warning |
| 同上 > 30 s | critical |
| `search_consistency_outcome_total{outcome="lagging"}` 占比 > 1%，持續 5 分鐘 | warning |

- **不自動 kill**。遷移、備份、批次匯入要事先登記 `application_name`，告警時能分辨出來。
- 應用程式自己的規則：大量更新一律分批，每批獨立交易，最慢一批 < 1 秒。§5.5 實測，分批時最慢一批是 0.43 秒。

### 4.8 藍綠重建

**generation**：

- 每個 generation 的索引目錄是 `index/gen-<uuid>/`。
- `index/CURRENT` 檔記錄目前服務中的 generation，用「寫暫存檔再 rename」原子更新。
- generation 的中繼資料放在 commit payload：
  - `generation`、`data_epoch`。
  - `schema_version`（例如加了 `search_rev`）。
  - `analyzer_version`（CJK 範圍、joiner、大小寫、compound 上限）。
  - `query_semantics_version`（權重、欄位加權）。
  - `cursor`、`complete_before_xid`。

**什麼時候重建**：

- 啟動時發現 schema、analyzer 或 query semantics 的版本和程式不同。
- 管理指令。
- payload 損壞。

**重建時舊 generation 照常服務。** 如果 analyzer 或 query semantics 版本變了，舊 generation 必須繼續用**舊版的查詢 tokenizer**。程式要同時保留兩個版本，直到切換完成。

**流程**：

1. **登記保留租約**：在 `outbox_cursors` 插一列 `consumer = 'search@<gen>'`。
   - 這需要新增 `state` 欄位（migration），值有 `active`、`bootstrapping`、`retained`、`retired`。
   - `state = 'bootstrapping'`，`last_xid` 設成目前所有有效 consumer 中最小的 `last_xid`。
   - 沒有任何 consumer 時，設成 `'0'`，也就是全部保留。
   - 登記、退役、GC 計算界線，三者用同一把 advisory lock 序列化，保證 GC 看得到這筆租約。
   - outbox 清理會被這列擋住（§4.9）。
   - **一定要在開快照之前**做。
2. **開快照**：開 `REPEATABLE READ READ ONLY` 交易，取 `F0 = pg_snapshot_xmin(pg_current_snapshot())`。
3. **掃描**：在同一個快照下掃描所有沒封存的 issue，寫進新索引。
   - 唯讀交易沒有 xid，**不會擋 frontier**（§4.7 實測）。
   - 但它會留住 MVCC 舊版本，所以掃描要快：先把原文串流出來，再在快照外慢慢 tokenize。
   - 100 萬筆的耗時由 E6 量。
4. **關快照**：commit 新索引，payload 的 `complete_before_xid = F0`、`cursor = (F0, 0)`。這個 generation 這時才宣稱自己完整包含 xid < F0 的狀態。
5. **追趕**：新 generation 用自己的 cursor，照 §4.2 從 `(F0,0)` 重播。舊 generation 照常同步。
   - 有些事件的狀態已經在快照裡了，重播一次也無害。
6. **切換**（較強的保證，〔依建議預設，待確認，§10 Q-S8〕）：
   - 先暫停舊 view 發布新的進度，記下 `(F_old, cursor_old)`。
   - 等新 generation 達到 `F_new ≥ F_old`，**而且** cursor 已經追過 `cursor_old`。
   - 健康檢查。這只是健康檢查，**不是正確性證明**；正確性靠快照 baseline、重播 cursor 和 frontier 協定。
     - baseline 剛完成、還沒開始重播時：文件數要等於同一個快照的未封存 issue 數。
     - 切換前：抽 200 筆比對 `search_rev`。不一致的，要能用還沒套用的事件解釋，否則就中止切換。
     - 抽查可見性欄位。
     - 跑 E1 的 smoke 查詢。不要求排名和舊 generation 相同，analyzer 改版時本來就會不同。
   - 切換由單一 coordinator 依序做，**不宣稱磁碟和記憶體同時原子切換**：
     1. 確認新 generation 的索引和 payload 已經持久化。
     2. 寫暫存檔、fsync、rename、fsync 目錄，更新 `CURRENT`。
     3. 發布新的 `ArcSwap<SearchView>`。

     第 2、3 步之間由舊 view 服務是允許的。這時 crash 的話，重啟會依 `CURRENT` 載入已經驗證過的新 generation。
   - 舊 view 由還沒結束的請求持有，請求結束後才釋放。
7. **保留舊 generation**：rollback 窗口 24 小時，期間不再同步。真的要回切，要先讓它重新追上。超過窗口就刪掉目錄和 consumer 那一列。

- token 綁的是 `data_epoch`，**不綁 generation**，所以重建後舊 token 仍然有效。
- 舊的 `rebuild()`（`delete_all_documents`，再對每張 issue 插 outbox）要移除。它會讓搜尋整段空白，還會在 100 萬筆時一次寫入 100 萬列 outbox。

### 4.9 Outbox 清理

清理用的界線：

- 刪除條件：`xid < 所有 state IN ('active','bootstrapping','retained') 的 consumer 的 durable cursor 中最小的 last_xid`，**而且** `created_at < clock_timestamp() - interval '1 hour'`。
- 這一小時是依交易開始時間算的額外保留，不是「commit 之後保留一小時」的保證。
- 計算界線時要拿 §4.8 那把 advisory lock。
- 只看 durable 進度（payload 寫回 PG 之後的 cursor），不看記憶體裡的進度。
- 刪 consumer 必須是明確的管理動作，不能因為很久沒更新就默默忽略它。
- 索引壞掉、需要的事件又已經清掉時，一律走 §4.8 的快照重建，不能假裝重播成功。

### 4.10 和 E6 的關係

E6（研究文件 §7：「10 萬筆 issue 時 tantivy 近即時延遲、寫入後立即讀到」）改成驗證本章的協定。開 `track_commit_timestamp = on` 並記錄設定。

1. 端到端的 commit → visible：p50、p95、p99、max。另外記 `commit_ack → satisfied`；兩者分開記，不共用名稱。
   - 寫入速率：10、100、500 tps。
   - 更新比例：新增 30%、更新 60%、封存 10%。
2. 帶 token 的搜尋：等待時間、lagging 比例；滿足時，符合條件的新資料是不是真的在結果裡。
3. 同一個 xid 超過 500 個事件（500、501、1,500）時，F 不能越界。
4. 交易亂序：先拿到 xid 的長交易比較晚 commit。
5. 長交易：同 database、跨 database（重現 §4.7）、prepared。
6. 故障注入：
   - index commit 之後、PG cursor 之前 crash。
   - reload 之前 crash。
   - payload 寫入失敗。
   - 只有 F 前進的空 commit。
7. 藍綠重建：期間的查詢延遲、追趕要多久、磁碟與記憶體峰值，以及 GC 和 bootstrap 互搶。
8. 固定條件：全文長度分布、segment 數和 merge 狀態、冷快取和熱快取。

E6 會決定 `wait_ms` 預設值和告警閾值是否合適；在那之前，這兩個數字只是互動用的預算，**不是同步 SLO**。

### 4.11 錯誤與測試清單

格式同 §3.7（頂層 `retry_after_ms`、`next_actions`，對照 02 §14.2）。

| code | 情況 | retryable | `retry_after_ms` | `next_actions` |
|---|---|---|---|---|
| `invalid_consistency_token` | 格式錯誤，或數字太大 | false | null | `fix_input`（不帶 token 重查） |
| `consistency_epoch_changed` | epoch 不符 | false | null | `fix_input`（不帶 token 重查；**不重送提案、不請人重新核准**） |
| `search_not_caught_up` | 只在 `on_lag: "error"` 時出現 | true | 有 | `wait_and_retry` |
| `search_unavailable` | 索引損壞、同步停了超過 60 秒 | true | 有 | `wait_and_retry`（不當成 `not_found`） |

測試：

- **frontier**：
  - 同一個 xid 剛好 500、501、1,500 個事件。
  - 剛好 500 筆、後面沒有事件時，多一次空查詢後 F 推到 H。
  - 零事件的交易拿到的 token 也能滿足。
- **SearchView**：
  - 觀察到 H 之後、發布 view 之前，等待者不能通過。
  - 等到之後不能換成另一個 reader。
- **重啟**：重啟後 F 和索引內容一致；PG cursor 領先 payload 時會重播。
- **純 SQL 查詢**：不等 tantivy，`consistency.status = not_applicable`。
- **lagging**：
  - 回應沒有 items，也沒有 cursor。
  - 模擬 agent 在 `items: []` 加 `complete: false` 時不回答「不存在」，也不重送提案、不請人重新核准（MCP 回歸測試）。
- **等待者**：
  - 上限、取消、資源釋放。
  - 偽造很大的 X 時只會等到 `wait_ms`。
- **outbox 涵蓋率**：每一種會改變索引內容的寫入路徑都有 outbox，包括：
  - issue 的 subject、description、project、tracker、private、author、assignee。
  - 封存。
  - key 變更。
  - 未來的 note 和欄位權限變更。
- **藍綠**：
  - bootstrap 時登記租約和 GC 互搶。
  - 新舊 F 相同、但 cursor 不同時，不能切換。
  - 舊 token 在切換後仍然有效。

## 5. D14 效能尾巴

### 5.1 環境與資料

| 項目 | 內容 |
|---|---|
| PostgreSQL | 18.6 Docker 容器，映像預設設定（`shared_buffers=128MB`、`work_mem=4MB`、`random_page_cost=4`、JIT on），和 E9 相同 |
| 主機 | Windows 11 桌機，背景有其他程式 |
| 資料 | E9 的產生器（種子 42）。100 萬筆 issue、`issue_cf` 1,770 萬列、1,000 個欄位 |
| schema 形狀 | **D14 正式形狀**，見下方 |
| 可見範圍 | 30 個專案，private 只看自己的，約 10.8 萬筆 |
| 計時 | 每個查詢先暖機 1 次，再跑 15 次取中位數（另有註明的除外） |

D14 正式形狀：

- 每個欄位一個 covering partial index `(field_id, v_x, issue_id) WHERE field_id = N`。
- list 存在 `v_ref`。
- `(field_id, v_ref)` 的 MCV 統計。
- 比較條件不帶 `IS NOT NULL`，欄位 id 用字面常數。

限制：

- 沒有冷快取測試，資料都在容器的 OS page cache 裡。
- 15 次的「p95」其實就是最大值。
- 沒有並行負載。
- 結論只看差距在 2 倍以上的項目。

### 5.2 估計誤差（`results/est.txt`）

| 條件 | 估計 | 實際 | 低估倍數 |
|---|---:|---:|---:|
| f1 num 熱門（21 種值）≥ 10 | 191,877 | 496,516 | 2.6× |
| f8 num 熱門連續值 100..300 | 9,479 | 160,194 | **16.9×** |
| f2 date 熱門 ≥ 2025-01-01 | 34,368 | 219,289 | 6.4× |
| f9 date 熱門，一整年 | 11,340 | 75,293 | 6.6× |
| f800 num 冷門範圍 | 276 | 1,290 | 4.7× |
| f806 date 冷門 ≥ 2024 | 298 | 1,229 | 4.1× |
| f3 ref 等值（有 MCV） | 397,027 | 424,582 | 1.07× |
| f1 有值（`v_num IS NOT NULL`） | 217,604 | 949,336 | 4.4× |

原本記的「3–5 倍」只量了 10 萬筆、比較集中的值。100 萬筆加上連續值時，低估到 17 倍。兩個條件的低估相乘之後，numdate 案例估 34 筆、實際 4,823 筆（142 倍）。

### 5.3 統計修正的嘗試（`results/stats_try*.txt`，以 f8 100..300 為例，實際 160,194）

| 做法 | 估計 | 結論 |
|---|---:|---|
| 基準 | 9,479 | — |
| `v_num` statistics target 1000 | 9,495 | 無效：問題在條件分布，不在樣本數 |
| partial expression index `((v_num+0)) WHERE field_id = 8`，查詢用同一個運算式 | 3,917 | 更差 |
| `CREATE STATISTICS ON (CASE WHEN field_id=8 THEN v_num END)`，查詢同時帶 `field_id = 8` | 6,345 | 無效：`field_id` 的選擇率又被乘了一次 |
| 同上，查詢只寫 CASE 運算式，並建同一個運算式的索引 `WHERE (CASE…) IS NOT NULL` | **172,301** | 估計準了。但 numdate 仍要 53 ms，因為另一個 date 條件還是低估；而且每個欄位都要一個 stats 物件，還得改查詢與索引規則 |

**結論：第一版不全面做逐欄位的範圍統計。** 全域統計描述不了「每個欄位各自的分布」。逐欄位修正雖然有效，但維護成本高，也不能單獨保證多條件 top-N 查詢選對計畫。第一版改用 §5.4 的執行策略。

### 5.4 多條件 + `ORDER BY id DESC LIMIT 50`（`results/q3.tsv`）

| 案例 | 條件 | 符合筆數 | S0 現行 EXISTS | S1 `(…) IS TRUE` | **S2 EXISTS 加 `OFFSET 0`** |
|---|---|---:|---:|---:|---:|
| three（E9 原本 216 ms 的案例） | ref=opt0、num≥10、date≥2025 | 6,478 | 51.5 | 193.6 | **9.8** |
| hot2 | ref=opt1、num≥13 | 10,142 | 1.6 | 132.9 | 1.8 |
| numdate | f8 100..300、f2≥2025 | 4,823 | 307.4 | 100.2 | **6.6** |
| num2 | f8 100..300、f1≥10 | — | 43.4 | — | **3.2** |
| hotcold | ref=opt0、冷門 f800 範圍 | 62 | **4.6** | 671.7 | 731.8 |
| rareint（兩個常見條件、交集稀少） | ref=opt4（10 萬）、f9 一季（1.9 萬） | 180 | 238.0 | — | 255.0 |

（單位 ms，中位數。hotcold_S1、S2 跑 5 次和 3 次，rareint 跑 7 次。）

各形狀的計畫（見 `results/explain/`）：

- **S0**：planner 自己選。低估時常常 hash join 兩個熱門欄位的整個索引，再 nested loop 回 issues。
- **S1**：本來想用 `IS TRUE` 阻止 pull-up，結果 PG 把 EXISTS 改成 **hashed SubPlan**：先把 16 萬筆 f8 整個讀進 hash，反而更慢。**不採用。**
- **S2**：`simplify_EXISTS_query` 遇到 `OFFSET` 就不簡化，所以 EXISTS 留下來變成逐列的 correlated SubPlan。執行時沿 `issues_pkey` 反向走，每列用 PK `(issue_id, field_id)` 探測，湊滿 50 筆就停。這是 **PG 18 才確認過的寫法**，升級大版本時要重新驗證。

**選擇性探測**（上限 10,001，只掃 index）：

| 條件 | 耗時 |
|---|---:|
| f1 ≥ 10 | 1.2 ms |
| f800 冷門 | 0.5 ms |
| f3=opt4 | 4.5 ms |
| f9 一季 | 11.1 ms（7 次裡有 1 次 143 ms，推測是 visibility map 被先前的更新弄髒，沒有細查） |

**策略**（第 3 輪和 GPT 收斂）：

1. **沒有 `q` 的篩選查詢直接走 SQL**，不經過 tantivy。現況已經是這樣，維持。
2. 策略選擇只用在同時符合下列條件的查詢；其他形狀照現行路徑：
   - 沒有全文 `q`。
   - `ORDER BY id DESC`。
   - 小 LIMIT（≤ 100）。
   - 條件可以拆成「至少兩個單值自訂欄位正向條件」的 AND。OR、NOT、is_null 不在範圍內。

   注意事項：
   - 現行 service 沒有 `q` 時是用 `updated_at DESC, id DESC` 排序。要把 `updated_at` 排序納入，得先補 `(updated_at, id)` 的索引與 keyset 設計並實測，在那之前一律走 S0。
   - 符合時，對每個條件做**上限 T+1 的探測**。T = 10,000，是**絕對值**，不是百分比。
   - **任何一個條件 ≤ T（數得到精確值）**：用 S0，讓 planner 從稀有條件出發。
   - **全部 > T**：用 S2。
   - 「全部常見」不代表交集大。rareint 就是反例：兩個策略都要約 250 ms。所以這只是挑起始策略的啟發式，不保證延遲。
3. 探測計數只用來挑策略，**不對外回傳，也不是可見結果數**。目前的欄位都是單值，所以列數等於 issue 數；將來有多值欄位時要改成「索引項目數」。
4. 整個請求共用一個服務端工作預算（deadline，預設 2 秒），涵蓋探測、初始策略和最多一次的切換：
   - 每條 statement 的 `statement_timeout` 用**剩餘預算**設定。
   - 初始策略逾時後，剩下的預算足夠（≥ 300 ms）才切換，而且切換之後**不會拿到新的完整預算**。
   - 在顯式交易裡要用 savepoint。
   - 預算用完還拿不到一頁，就回 `query_timeout`。**v1 不回部分頁**。
   - 不承諾回應的牆鐘時間嚴格小於 deadline，因為取消和清理也要時間。
5. 不建 hot projection，不改 schema。

改了策略之後的效果。「SQL 形狀」是實測值；「加上探測」是把各條件的探測中位數加總的**估算**，端到端還沒實測：

| 案例 | 原本（S0） | 選到的形狀 | SQL 形狀 | 加上探測（估算） |
|---|---:|---|---:|---:|
| E9 案例 three | 51.5 ms | S2 | 9.8 ms | 約 16 ms |
| numdate | 307 ms | S2 | 6.6 ms | 待量測（f8 和 f2 的探測沒有單獨量） |
| num2 | 43 ms | S2 | 3.2 ms | 待量測 |
| hotcold | 4.6 ms | S0（f800 ≤ T） | 4.6 ms | 約 5–10 ms |
| rareint | 238 ms | S2（兩個都 > T） | 255 ms | 約 270 ms；比 S0 慢一點，靠 deadline 兜底 |

### 5.5 總數（count）

| 查詢 | count(*) |
|---|---:|
| three | 200.7 ms |
| hot2 | 196.7 ms |
| num2 | 497.0 ms |
| numdate | 307.0 ms |
| rareint | 237.8 ms |

**第一版不回精確總數**，只用「多取 1 筆」判斷有沒有下一頁。SPA 顯示「已載入 50 筆，還有更多」，不顯示「共 50 筆」。〔依建議預設，待確認，§10 Q-S6〕

### 5.6 慢查詢怎麼呈現

- **SPA**：超過 400 ms 顯示「正在套用欄位條件…」和「取消」。到 deadline 時顯示：「這組條件太廣，請加上專案或日期範圍。」附可以一鍵加入的篩選建議。**不偷偷縮小範圍**。
- **MCP**：回 `query_timeout`。

```json
{"code": "query_timeout", "request_id": "…", "retryable": false,
 "retry_after_ms": null, "next_actions": ["fix_input", "ask_human"],
 "message": "欄位條件太廣，請縮小範圍",
 "details": {"deadline_ms": 2000, "suggestions": ["add_project_filter", "narrow_date_range", "add_text_query"]}}
```

`retryable: false` 的意思是「原樣重試通常沒用」，不是不能再查。

### 5.7 改型別的遷移流程

現況：觸發器 `field_defs_freeze_type` 禁止改型別（和 Redmine 一致）。**要不要支援，是產品決策**〔§10 Q-S9，已決定 2026-11-07：不就地改型別，提供「建新欄位 + 複製轉換 + 停用舊欄位」的管理工具〕。

建議預設是「不支援就地改型別」，改用下面這個組合流程：

1. 建新欄位。
2. 執行「複製並轉換」的管理工具。
3. 停用舊欄位。

選擇支援就地改型別時，採下面的**維護窗口**方案。GPT 第 1 輪提的版本化定義加上 dual-write，第 2、3 輪雙方都認為第一版太重。

**實測**（`results/retype*.txt`，不含 `issue_cf_check` 觸發器的成本）：

| 動作 | 數量 | 耗時 |
|---|---:|---:|
| 分批就地轉換 num → text（每批 12,500 張 issue、約 1 萬個值，各自 COMMIT） | 798,071 值 / 80 批 | **13.1 s**，最慢一批 0.43 s |
| `CREATE INDEX CONCURRENTLY` 新的 `(field_id, v_text, issue_id) WHERE field_id = 8` | — | 9.1 s |
| 對照：單一 UPDATE 轉換 | 949,336 值 | 35.3 s（整段持有 xid，擋住搜尋 frontier） |

`issue_cf` 這張表沒有任何更新是 HOT，因為值在 covering index 裡。轉換多少個值，就留下多少個 dead tuple。

**流程**：

1. **預檢（dry run）**：
   - 依具名的轉換規則數出無法轉換的值，例如 `strict_decimal`、`iso_date`、`option_by_exact_label`。
   - 報告也套欄位可見性：讀不到這個欄位的人，看不到失敗的樣本。
   - 預設 `invalid_policy: reject`。有無法轉換的值就不開始，除非管理員明確選擇「清空並記錄」。
2. **進入維護**：
   - `field_defs.index_state = 'migrating'`，加上 `migration_target_type`。這是新的狀態，要改 CHECK。
   - 鎖定方式比照 `dropping`：
     - 一般寫入方在交易裡對 `field_defs` 那一列取 `FOR SHARE` 並檢查狀態（`issue_cf_check` 現在就是這樣做）。
     - 進入維護的交易對同一列 `UPDATE`，這個列鎖會和 `FOR SHARE` 衝突，所以會等現有的寫入方結束。
   - 進入 `migrating` 之後**重新做一次預檢**，因為 dry run 到這時之間可能有新值寫入。
   - 之後一般寫入一律拒絕。只有帶著目前 migration id、而且有專用資料庫角色的 migration worker 可以寫入轉換結果；不靠呼叫者可以任意設定的 session 變數放行。
   - 讀取時，狀態檢查和讀值要在同一個 SQL 快照裡，避免先看到 active、接著卻讀到轉了一半的資料。
   - **維護期間這個欄位不能寫、不能篩選、不能排序**：
     - 寫入回 `field_migrating`。
     - 篩選和排序回 `field_migrating`。
     - `get` 回 `{state: "migrating", value_available: false}`，**不能回 null 假裝沒有值**。
3. **分批轉換**：
   - 依 issue id 分批，每批一個短交易，最慢一批要 < 1 秒。
   - 觸發器在 `migrating` 狀態下要接受目標型別的欄位。
   - 進度記在 `field_migrations(field_id, last_issue_id, converted, failed, state)`，crash 之後從 `last_issue_id` 接著做。
4. **建索引**：
   - 用 `CREATE INDEX CONCURRENTLY` 建 `issue_cf_f<id>_v<n>`。名稱要帶版本，因為舊索引還在。
   - 建完要確認 `pg_index.indisvalid`。失敗留下的無效索引交給 `reconcile_indexes` 清掉。
5. **切換**：
   - 短交易：更新 `ftype`，`definition_version + 1`，`index_state = 'ready'`。
   - 之後舊版本的篩選回 `field_definition_changed`，舊版本的 citation 回 `source_changed`。
6. **清理**：
   - `DROP INDEX CONCURRENTLY` 舊索引。
   - 提醒管理員：手動 VACUUM，或等 autovacuum 回收 dead tuple。

失敗時：

- 狀態維持 `migrating`，**不自動回到 active**。
- 第一批資料變更之前：可以 `abort`。
- 開始變更之後：預設只提供 `resume`。只有下面兩種情況才提供 rollback：
  - 已經把原值另外保存。
  - 這個具名的轉換已經證明可逆。
- 不可逆的轉換（降精度、文字轉數字），以及 `invalid_policy: clear`，要嘛另外保存原值，要嘛由管理員明確接受「開始之後不能無損中止」。

### 5.8 測試清單

- 策略選擇：
  - 探測回 T+1 時，不能當成精確的計數。
  - 任一條件 ≤ T 時用 S0，全部 > T 時用 S2。
  - 回歸案例：hotcold 不能選到 S2，three 和 numdate 要選到 S2。
- 計畫結構：
  - S2 的計畫是 `Index Scan Backward using issues_pkey` 加 correlated SubPlan，**不能是 hashed SubPlan**。
  - S0 不檢查固定計畫。
  - 不比對整份 EXPLAIN 字串。
- deadline：
  - S2 逾時後確實會被取消。
  - 切換時不會拿到新的完整預算。
  - 不會留下還在背景跑的查詢。
  - 量測取消和清理的延遲。
  - 在交易裡用 savepoint 正確 rollback。
- 範圍外的形狀不走策略選擇，包括：有 `q`、`updated_at` 排序、OR、NOT、is_null。
- 分頁：S0 和 S2 結果完全相同（同一份資料、同一個快照），分頁不漏不重。
- 偏斜資料：
  - 可見專案集中在最舊的資料（S2 的最壞情況）。
  - 熱門值集中在讀不到的專案。
- 改型別：
  - 第 1 批、第 N 批之後 crash，再 resume。
  - 維護期間讀取不會回假的 null。
  - concurrent build 失敗時留下的無效索引會被清掉。
  - 舊的 filter 和 citation 會收到 `field_definition_changed`、`source_changed`。
- PostgreSQL 升級大版本時，重跑 §5.4 的全部案例。

## 6. 使用者方便性檢查表

編號用 `UX-` 前綴，和設計 02 的待決問題 U1–U13 區分。

| # | 情境 | 必須做到 | 驗收方式 |
|---|---|---|---|
| UX-1 | 搜尋結果列表 | 每筆一眼看得出為什麼命中，見下方 | 5 位使用者看 10 筆結果，能說出命中原因的比例 ≥ 90% |
| UX-2 | 只有部分字元匹配 | 標「部分字元相符」，淡色高亮，不叫「高度相關」 | 視覺檢查；`strength: fallback` 一定有對應文字 |
| UX-3 | 純欄位篩選 | 列出命中條件的欄位值（例如「Severity：High　估時：12 h」），不顯示假的全文片段 | — |
| UX-4 | 剛建立的單 | 寫入回執（`/commands` 回應或 approve-and-apply 的 execution，02 §6.9）一回來就顯示「已建立 AUTH-124」；搜尋清單上方另外放一張「剛建立・搜尋同步中」卡片，能直接開啟或引用；**不能只因為搜尋延遲就顯示「建立失敗」或「找不到」**（真的寫入失敗、之後被刪除或撤權，依實際狀態顯示） | E2E：模擬索引延遲 3 秒，畫面上沒有失敗字樣 |
| UX-5 | 搜尋同步中 | 一行小字說明，不跳警告視窗，見下方 | — |
| UX-6 | 欄位篩選慢 | 400 ms 後顯示進度和「取消」；到 deadline 時給一鍵縮小範圍的建議，不偷偷改條件 | — |
| UX-7 | 沒有總數 | 顯示「已載入 50 筆，還有更多」，不顯示會誤導的總數 | — |
| UX-8 | 結果不完整 | `truncated` 或 `complete: false` 時顯示「結果可能不完整，請縮小條件」，不能只顯示一個空列表 | — |
| UX-9 | 引用失效 | 驗證時回 `source_changed`，就顯示「原文已修改」和「查看目前內容」，不靜默刪掉引用 | — |
| UX-10 | 欄位維護中 | 篩選器上的欄位標「維護中」，值顯示「暫時無法顯示」，不顯示空白 | — |
| UX-11 | 無障礙 | 高亮不能只靠顏色（加底線或粗體）；primary 和 fallback 有不同的文字標示；螢幕閱讀器會念出「命中：登入」 | axe 檢查與螢幕閱讀器抽查 |
| UX-12 | 錯誤訊息 | `invalid_filter`、`ambiguous_reference` 在 UI 上直接提供修正選項（候選欄位、合法值） | — |

UX-1 的細節：

- 顯示命中的欄位名稱（標題或描述）。
- 片段約 60 字，命中的詞以高亮顯示。

UX-5 的說明文字：「搜尋清單同步中，通常 1 秒內完成」。有 E6 的數字之前，不寫具體秒數，改寫「稍後」。

## 7. 實作順序與每步完成標準

| 步 | 內容 | 完成標準 | 依賴 |
|---|---|---|---|
| 1 | spanned lexer：`Lexeme{term, kind, from, to}`，tokenizer 補上 offset | `cargo test` 全綠；E1 語料全文的 term 序列和舊版逐筆相同；offset 屬性測試通過（§3.9 前 4 項） | 無 |
| 2 | evidence 與 `search(explain, fields)`；原文比對淘汰（不受 `explain` 影響）；拿掉 `score`，改回 `rank`、`match`；`invalid_query` | 測試：<br>· §3.9 的 CJK、識別碼、Unicode、預算、分數、索引不同步測試。<br>· E1 的 nDCG 和 D15 一致（排名沒動）。<br>· 用 Claude 的 tokenizer 量用法 A 到 E 的回應大小並記錄（當作基準，不當門檻） | 1 |
| 3 | `citation_id`（`issue_field`、`issue_note`）、`verify_citations`、`get_source`、`get(fields, citations)`；agent 憑證套用委派讀取範圍；錯誤帶頂層 `retry_after_ms`、`next_actions` | §3.9 的權限、引用驗證測試；「找不到」和「沒權限」的回應位元組相同；agent 憑證對私有 issue、私有 note 的 citation 回 `not_found`；每個錯誤碼的 `next_actions` 和 §3.7、§4.11 的表一致 | 2；agent 部分要 02 的委派（02 §3.5） |
| 4 | 同步迴圈重構：單一擁有者、H 和事件同一個 statement、SearchView 加 F、commit payload、等待者 | §4.11 的 frontier、SearchView、重啟測試；kill -9 注入測試通過 | 無（可以和 1–3 並行） |
| 5 | `consistent_after`、`wait_ms`、`on_lag`，純 SQL 分流，epoch 表 | §4.11 的 lagging、等待者、純 SQL 測試 | 4 |
| 6 | 寫入回執帶 token（02 §6.9）：寫入交易裡把 epoch 和 xid 一起存成完整 token | 用法 D 的 E2E：`changeset_propose` → 人在 SPA 核准 → `changeset_get` 取 `effects[].key` 和 `search.consistent_after` → 立即 `get(citations)` → 帶 token 搜尋，全部成立；新建 issue 的 `lock_version` 是 0；輪替 epoch 後 `changeset_get` 仍回原 token，搜尋回 `consistency_epoch_changed`；模擬延遲時 agent 不重送提案、不請人重新核准 | 5，加上 02 的 ChangeSet 與 `/commands` |
| 7 | 指標與長交易告警 SQL | §4.6 的指標可以在 `/metrics` 看到；重現 §4.7 的跨 database 交易時，告警會觸發 | 4 |
| 8 | generation 與藍綠重建；`search_rev` STORED；移除舊的 `rebuild()` | §4.11 的藍綠測試；重建 10 萬筆期間搜尋不中斷 | 4 |
| 9 | evidence 用上 `search_rev`：回 `rechecked` 狀態；`search_rev` 相同時跳過原文比對 | `search_rev` 相同和不同兩種路徑的結果一致 | 2、8 |
| 10 | D14 策略選擇：探測、S0 和 S2、共用 deadline、`query_timeout` | §5.8 的策略、計畫結構、deadline、分頁測試。另外用 `experiments/sd` 重建資料跑 benchmark，記錄完整環境，回報探測、初始策略、切換、端到端的中位數與 p95，要求兩件事：<br>① 指定案例選對策略（three、numdate 選 S2，hotcold 選 S0）。<br>② 相對 S0 有改善。<br>絕對毫秒數只當參考，不當 CI 門檻 | 無 |
| 11 | `resolve(kind: "custom_field")`：型別、單位、選項、`definition_version`；`field_definition_changed` | 用法 C 的 E2E | 無 |
| 12 | E6 | §4.10 全部跑完並寫成報告；回頭調整 `wait_ms` 和告警閾值 | 4–8 |
| 13 | 改型別的維護流程（Q-S9 已決定不就地改：本步改成「建新欄位 + 複製轉換 + 停用舊欄位」工具） | §5.8 的改型別測試；在 100 萬筆資料上重量，含觸發器成本 | 10、Q-S9 |
| 14 | private notes 和受限欄位進索引（只有 Q-S1、Q-S2 決定之後才做） | 嚴格契約的測試：新增看不到的文件，不能改變看得到的結果的排序 | Q-S1、Q-S2 |

第 1–3 步是「search explain 縮小版」，第 4–6 步是「寫入後一致性」，這兩組就是第一切片要的。第 7–11 步可以排在第一切片之後的下一個迭代。

## 8. 和其他文件的對照（之後回寫，這次不改）

- 01 §10 的「search explain」和「寫入後的搜尋一致性」：可以補上連到本文的連結。
- 研究文件 §7 的 E6：範圍改成 §4.10。
- 研究文件 D11：補上「告警看 `backend_xid`，跨 database 也算」和「大量更新要分批」。
- [`research/02-e9-custom-fields.md`](../research/02-e9-custom-fields.md) §8 的結論（不是設計 02）：「範圍低估 3–5 倍」改成「100 萬筆時 2.6–17 倍」，並補上 §5.3、§5.4 的實測。

### 8.1 和設計 02 的對齊（2026-11-07，02 附錄 C.3）

| # | 02 C.3 的要求 | 本文修改的位置 |
|---|---|---|
| 1 | agent 不能直接寫入；用法 D 改走提案與核准 | §0、§2.1 工具表、§2.5 用法 D、§4.4 工具描述、§7 第 6 步 |
| 2 | 寫入回執形狀引用 02 §6.9，拿掉〔假設介面〕 | §4.3；文件開頭的範圍說明 |
| 3 | 新建 issue `lock_version` 是 0 | §2.5、§4.3 |
| 4 | `retry_after_ms` 移到頂層，每個碼加 `next_actions` | §2.7 第 3 點、§3.7、§4.4、§4.11 |
| 5 | `search.fields` 加 `lock_version`、`priority`（U3 採用時）、`created_at` | §3.6 |
| 6 | `get.fields` 合法值、和 `include` 並存、`get_many` 同參數 | §2.1、§3.6「`get` 的 `fields`」 |
| 7 | note 的 citation `src.kind` | §3.6 的 `src.kind` 表（`issue_note`） |
| 8 | agent 呼叫者範圍＝委派讀取範圍，引用工具都套用 | §3.6 驗證順序第 2 步、§3.8「呼叫者的讀取範圍」、§3.9 |
| 9 | 方便性檢查表改 `UX-` 前綴 | §6、附錄 A |
| 10 | 「02 §8」寫明是 `research/02-e9-custom-fields.md` | 本節上方 |
| 11 | epoch 和 xid 在交易內一起持久化成完整 token | §4.3、§7 第 6 步 |

11 項都照改，沒有認為 02 有錯而不改的項目。第 5 輪複審另外修了 13 處（附錄 A）。

### 8.2 回饋 02（請契約線決定，本文沒有改 02）

| # | 02 位置 | 建議 | 理由 |
|---|---|---|---|
| F1 | §8.8 `get_many` | 參數補上 `event_limit`，或寫明 `get_many` 不支援 `recent_events` | 現在 `get_many` 宣稱和 `get` 相同的 `include`，卻少一個限制參數 |
| F2 | §4.6 第 509 行 | MCP 的領域錯誤寫成 `{code, request_id, retryable, details}`，少了 §4.3 的 `retry_after_ms`、`next_actions`、`message`；改成「和 §4.3 同一個錯誤物件，只是沒有 `{"error": …}` 外層」 | 同一份文件內兩種形狀，agent 會拿不到 `next_actions` |
| F3 | §14.2 `source_unverifiable` | 寫明它是 propose 驗證 evidence 的外層錯誤，`details` 帶 03 §3.7 的逐筆狀態（`not_found`、`source_changed`、`citation_mismatch`、`ambiguous`、`invalid_citation`）；03 的 `moved`、`ambiguous` 等逐筆狀態只由 03 維護 | 現在兩邊各有一部分，對照不完整 |
| F4 | §8.8 `changeset_get`、`operation_get` | 寫明 agent 讀 execution、effects、validation 時也套用 §3.5 的委派讀取範圍，而且委派失效後回 `delegation_unavailable` | 回執裡的 key、validation 細節可能指向目前已讀不到的來源；不能因為是回執就繞過撤銷 |
| F5 | §8.8 `get` 的 `include: ["actions"]` | agent 呼叫時只列 agent 能**提案**的動作，不列委派者能直接做或核准的 | 否則 agent 會誤以為自己能執行 |
| F6 | §6.9、§3.7 | 定義兩個並行邊界：委派有效性檢查的快取與在途請求怎麼處理；epoch 輪替的維運流程要先停寫入或等在途交易結束，再切換索引 | 交易內保存完整 token 解決了重組問題，但沒涵蓋輪替當下的在途交易 |

**2026-11-07 補（02 附錄 C.5 #14–17，U1 改成多專案委派）**：agent 讀取範圍改成工作脈絡內有效委派專案的聯集（§1 開頭、§3.8）；每次請求先檢查脈絡 active 與 R 的 epoch（§3.8）；錯誤碼加 `context_unavailable`（§3.7）；搜尋可能跨多個專案（§3.8、§3.9 測試）。

**2026-11-07 處理結果**：F1–F6 契約線全部採用（02 附錄 C.4）。F6 的做法：授權狀態不跨請求快取；寫入交易對 `search_settings` 列加 `FOR SHARE`，輪替自然等在途寫入結束，不必停寫入。02 回頭要求 03 補的 #12、#13 已寫進 §4.3（epoch 輪替的協調點）。

## 9. 風險

1. **S2 依賴 PG 18 planner 的行為**（`OFFSET 0` 會阻止簡化）。升級大版本時必須重跑 §5.4。
2. **啟發式選錯策略**：例如兩個條件都常見、交集卻稀少。靠 deadline 兜底，最壞會回 `query_timeout`。
3. **基本契約的殘餘風險**：排名、策略選擇和耗時都可能反映看不到的資料分布，§10 Q-S1 要讓使用者清楚知道。
4. **outbox 漏登記**：frontier 再正確，也救不了沒寫 outbox 的變更。靠涵蓋率測試。
5. **不簽章的 citation 和 token**：沒辦法證明「伺服器發過」。如果之後的需求是稽核級引用，要升級成 `cit2`。

## 10. 待使用者決定

> 依 2026-11-06 使用者指示「一路做到設計完成」，以下每一題都先照「建議預設」寫進文件，相關段落標了〔依建議預設，待確認〕。不回答就維持建議預設。

| # | 問題 | 建議預設 | 不採用預設的影響 |
|---|---|---|---|
> **2026-11-07 使用者已決定**：Q-S1 選基本契約；Q-S9 不就地改型別、提供轉換工具。其餘各題維持建議預設。

| Q-S1 | **排名契約**：接受「看不到的文件可能影響排序（不洩漏內容）」的基本契約，還是要求嚴格契約？ | 第一版接受基本契約，文件明示；拿掉 `score` | 嚴格契約：要做可讀來源 allowlist、改評分方式、重跑 E1，至少多 2 個實作步驟 |
| Q-S2 | private notes 與受限自訂欄位的**全文搜尋**能不能延後？ | 延後，先用 SQL 讀取和結構化篩選 | 要第一版就搜得到：必須先定 Q-S1，並做子文件聚合 |
| Q-S3 | 引用要證明的是「目前存在」，還是「伺服器曾經回傳」或「歷史上存在」？ | 第一版只證明目前存在而且讀得到（`cit1` 不簽章） | 要可稽核的歷史引用：需要簽章（`cit2`）加上不可變的來源版本（依賴 issue_events 保存舊值） |
| Q-S4 | 搜尋等不到剛寫入的資料時，預設回可處理的狀態（`search_lagging`），還是回錯誤？ | MCP 和 REST 都預設 `return`；`error` 可選 | 預設 `error`：agent 比較不會誤把空結果當完整，但比較容易把它當成整個任務失敗 |
| Q-S5 | 搜尋同步的**產品 SLO** | 暫定：帶 token 時 `wait_ms` 預設 1 秒、上限 2 秒；E6 之後再定 SLO | 給定 SLO 後，告警閾值和 `wait_ms` 跟著調 |
| Q-S6 | 需不需要**精確總數**？ | 不提供，只顯示「還有更多」 | 要總數：本次測試資料和環境下每次多 200–500 ms（參考值，不是 SLA），或做非同步計數 |
| Q-S7 | 欄位篩選太慢時，可不可以要求使用者縮小範圍（`query_timeout`），而不是一定跑完？ | 可以，deadline 預設 2 秒 | 要一定跑完：需要非同步查詢服務 |
| Q-S8 | 重建切換時，要不要求「不倒退舊 view 已套用的事件」？ | 要求：新 generation 的 cursor 追過舊 cursor 才切換；不要求排名不變 | 只要求 F 不倒退：切換比較快，但可能短暫少幾筆剛更新的內容 |
| Q-S9 | **要不要支援自訂欄位改型別**？能接受該欄位的維護窗口嗎？ | 不支援就地改型別；提供「建新欄位 + 複製轉換 + 停用舊欄位」的管理工具。若要就地改：接受該欄位暫停讀寫。本次環境下 80 萬個值約 22 秒（轉換 13.1 秒加索引 9.1 秒），這是參考值，不是 SLA；正式流程和觸發器的成本還沒量 | 不接受窗口：需要版本化定義加 dual-write（GPT 第 1 輪方案），實作量大很多 |
| Q-S10 | 剛建立的單要不要出現在搜尋頁？ | 顯示一張獨立的「剛建立・搜尋同步中」卡片，不冒充搜尋命中 | 不顯示：使用者只能從建立成功的提示開啟那張單 |

**相依**：

- Q-S1 → Q-S2：先定排名契約，才能決定要不要索引受限內容。
- Q-S3 依賴寫入設計的 issue_events 有沒有保存舊值。
- Q-S5 依賴 E6 的結果。
- 其餘各題獨立。

## 附錄 A · GPT 討論紀錄

全部用 `mcp__plugin_aura_gpt__ask`，模型 gpt-6.1-sol，effort high，thread `oxledger-search-consistency-explain`。附檔只有 `crates/oxledger/src/search.rs` 和 `migrations/0001_init.sql`（第 1 輪），以及本文件草稿（第 4 輪）。

### 第 1 輪：獨立提方案（call `20261106-224930-5016a5`）

只給現況，不給 Claude 的方案。GPT 的重點：

- **三種保證**：引用有效、搜尋追上寫入、跨系統快照，前兩種做、第三種不做。這個框架保留成本文 §0、§1.2。
- **explain**：
  - 不用 `Explanation`，也不先改成 STORED。
  - 做共用的 spanned lexer，從 SQL 原文產生「match evidence」。
  - 引用用 HMAC 簽章，帶 `source_revision`；offset 用 UTF-8 byte。
  - 錯誤用大寫代碼，例如 `SOURCE_UNAVAILABLE`。
- **排名側通道**：指出 BM25 全域統計會受看不到的文件影響，提出基本契約和嚴格契約之分。
- **一致性**：
  - 簽章的 `(data_epoch, pipeline, xid8, outbox_id)` barrier。
  - 逾時回 503 `SEARCH_NOT_CAUGHT_UP`，絕不回空陣列。
  - reader 要和 watermark 綁在一起。
  - watermark 存在 tantivy commit payload（它不確定 0.27 有沒有這個 API）。
- **長交易**：說「已分配 xid 的交易是主要嫌疑；唯讀交易不一定會擋」，跨 database 的影響要實測。
- **藍綠重建**：REPEATABLE READ 快照、bootstrap `(H,0)`、retention lease、generation metadata。
- **D14**：
  - 先確認是不是錯誤計畫造成的，不為了估計誤差本身改 schema。
  - 建議 ordered candidate scan 加 LATERAL probe、selective-set-first 兩條路徑。
  - 熱門欄位用 hot projection。
  - 改型別用版本化定義加 dual-write。

**Claude 的核對**：

- tantivy 原始碼與探針程式：
  - `SnippetGenerator` 的 offset 問題、byte 計算、忽略 boost。
  - `Explanation` 有 `N` 和 `n`。
  - commit payload API 存在而且可用。
- `frontier.sh`：跨 database、已分配 xid 的交易會擋；唯讀交易不會。
- 讀 `main.rs`：只有一個同步 loop。所以 GPT 說的「並行 sync 會倒退 cursor」現在不會發生，但仍採用「單一擁有者」作為型別上的保證。

### 第 2 輪：交換方案、互相批判（call `20261106-230226-cb7c5b`）

Claude 提出 8 點，加上使用者新增的要求（方便性、MCP 主要用法）：

1. token 只放 xid，搭配排他 frontier。
2. token 不簽章。
3. 逾時回 200 加 lagging，並帶 `pending_writes`。
4. citation 不簽章，`rev` 取 12 個 hex。
5. 對外用 code point。
6. 錯誤碼對齊 01 §15.7。
7. 拿掉 score，採基本契約。
8. hot projection 和 dual-write 對第一版太重。

GPT 的回應：

| 點 | GPT 的立場 | 結果 |
|---|---|---|
| token 只放 xid | 同意，比自己的版本乾淨。補充四點：內部 cursor 仍要 `(xid,id)`；滿 500 筆時 F 只能是 `last.xid`；零筆時也要拿到 H；F 必須在 view 發布之後才生效 | 採用（§4.1、§4.2） |
| token 不簽章 | 同意，但 token **不能證明 commit**；epoch 不能宣稱自動辨識復原分支，要寫進維運手冊 | 採用（§4.3） |
| 逾時行為 | 改成支持 MCP 預設回 lagging，但反對 `pending_writes`：只有 xid，推不出是哪些單 | 採用 GPT 的修正：不回 `pending_writes`，「剛建立」卡片由 SPA 自己的回執產生（§4.4、§6 UX-4） |
| citation 不簽章 | 同意「目前可驗證」不需要 HMAC；但 12 個 hex 太短，要用完整 sha256；`moved` 要明確 opt-in（`reanchor`），不能靜默轉成 `valid`；用 UUID 不用 key | 採用（§3.6） |
| code point | 同意。citation 以 code point 為準，片段顯示時往外延伸到 grapheme 邊界 | 採用（§3.5） |
| 錯誤碼 | 同意 | 採用（§3.7） |
| 基本契約 | 同意，但要寫明拿掉 score **不等於**解決側通道 | 採用（§3.8），列 Q-S1 |
| hot projection 等 | 同意第一版不做；建議先讓「無 q 走 SQL」「確認 partial index 和 prepared plan」「bounded ordered probe」 | 採用（§5.4） |

GPT 另外寫了 5 個 MCP 用法的序列，§2 以它為底，做了兩處修正：

- `status_category` 現在就有 `"open"`，不需要陣列。
- 用法 A 的 tracker 直接傳名稱，不必先 resolve，所以往返從 2 次減為 1 次。

### 第 3 輪：收斂與一致性審查（call `20261106-231339-769d13`）

附上 D14 的全部實測。GPT 指出的必修項，以及結果：

1. **探測上限 10,001 和 2 萬的門檻不一致**：上限 10,001 只能知道「≥ 10,001」。→ 改成絕對值 T = 10,000，上限 T+1（§5.4）。
2. **單一條件的數量不等於交集**：→ 寫明是啟發式；Claude 補量了 rareint 反例：兩個策略都要約 250 ms（§5.4）。
3. **就地分批轉型時只禁寫不夠**：轉了一部分之後，讀取會看到值「消失」。→ 維護期間欄位不能讀、寫、篩選，`get` 回 `value_available: false`（§5.7）。
4. **藍綠重建要先登記 retention lease，再開快照**：→ 採用（§4.8 步驟 1）。
5. **`search_rev` 和 `source_rev` 要分開**：→ 採用（§3.2）。
6. **`complete`、`truncated`、lagging、`evidence_truncated` 是不同概念**：→ 採用（§4.4 表）。
7. **其他**：
   - S2 超時換 S0 要共用一個 deadline，最多換一次。
   - 純 SQL 查詢的一致性回 `not_applicable`。
   - 切換時要追過舊的 cursor，不只是 F。
   - frontier 停滯只在有待處理事件或有等待者時才告警。
   - `OFFSET 0` 要標成 PG 18 才確認過的寫法。
   - 結論不要寫成「修統計無效」。

   以上全部採用。
8. **產品問題清單**：→ 併入 §10（Q-S1–Q-S10）。

### 第 4 輪：全文一致性審查（call `20261106-232715-02c6eb`）

附上本文草稿。GPT 依嚴重度列出 20 項，**全部採用**，其中第 1 項先實測確認。逐項處理如下：

| # | 問題 | 處理 |
|---|---|---|
| 1 | §4.2 的 SQL 在零事件時，`LEFT JOIN` 會產生一筆全是 null 的 event | **PG 18.6 實測確認**：沒有 `FILTER` 時回 `[{"id": null}]`。已加上 `FILTER (WHERE ev.id IS NOT NULL)` |
| 2 | cursor 和 frontier 沒有分開「暫定、durable、served」三種狀態 | 已補（§4.2 步驟 4–6） |
| 3、4 | 改型別時 `FOR SHARE` 不互斥、migration worker 例外沒定義、`abort` 不一定可逆 | 已補鎖定方式、worker 權限、resume 與 rollback 的條件（§5.7） |
| 5、6 | `get` 回全文 citation 卻沒有選子引文的 API；上下文 citation 可能超過驗證上限 | 新增 `selection: range｜subquote`；context 不再是可引用範圍；統一 2,000 code point 上限（§3.6） |
| 7、8、9 | 租約和 GC 沒有互斥；`CURRENT` 和 `ArcSwap` 不可能同時原子更新；健康檢查要求 SQL 和索引精確相等 | 已改：advisory lock、coordinator 依序切換、抽樣只當健康檢查（§4.8、§4.9） |
| 10、11、12 | 策略範圍比實測寬（`updated_at`、有 `q`）；deadline 保證太強；效果表沒算探測 | 範圍限縮到 `id DESC` 無 `q` 的 AND；用剩餘預算；表格分開實測與估算；§7 改成相對改善（§5.4、§7） |
| 13 | 原文比對不能看 `explain` 開關，也沒辦法只讀最後一頁 | 改成分批延後載入，比對一律執行（§3.4） |
| 14 | cursor 的版本與失效規則 | 新增 `invalid_cursor`、`cursor_expired`；`rank` 定義為跨頁序號（§3.6、§3.7） |
| 15、16 | 空白 `q`、純 SQL 的 `match`、token 驗證順序；時間預算到底回錯誤還是不完整頁 | 已統一：時間預算用完一律回 `query_timeout`（§3.4、§4.4） |
| 17 | `details.allowed` 會洩漏受限欄位；快取沒考慮權限撤銷 | 已補（§3.8） |
| 18 | MCP 範例沒帶 `definition_version`；預設值和批次錯誤範圍沒寫清楚 | 已補（§2.4、§3.6、§3.7） |
| 19 | 只有 fallback 時不知道怎麼選片段 | 已補 tie-break 規則（§3.5） |
| 20 | 摘要和驗收有超額承諾（唯讀交易、1,200 token、UX-4、Q-S6、Q-S9） | 措辭已改；數字標成本次環境的參考值 |

第 4 輪之後沒有剩下的分歧，所以沒有再加輪。

### 第 5 輪：複審「和設計 02 對齊」的改動（2026-11-07，call `20261107-000516-6d8293`）

只送改動部分的說明加上全文，請 GPT 分成 must-fix、should-fix、回饋 02 三類。逐項核對 02 原文後處理：

| GPT 的意見 | 核對 | 處理 |
|---|---|---|
| 錯誤外殼不一致：§3.7 有 `{"error": …}` 外層，§5.6 範例缺共同欄位 | 02 §4.3 的 REST 有外層，02 §4.6 的 MCP 沒有 | 採用：§3.7 改成只寫錯誤物件，說明 REST／MCP 各自怎麼包；§5.6 範例補齊欄位 |
| `get_many` 漏 `event_limit` | **不採用**：02 §8.8 的 `get_many` 本來就沒列 `event_limit` | §3.6 照 02 寫，缺口列進回饋 02 |
| `issue_note` 要驗父子關係 | 成立：citation 不簽章，可以偽造 `issue_id` 加別張的 `note_id` | §3.6 規定查詢同時限制 `note.issue_id`；§3.9 加測試 |
| 「同一個 `Pred`」涵蓋不了 note 和自訂欄位的限制 | 成立 | §3.8 改成「同一個授權上下文」，分層說明 |
| 批次整體失敗沒包含委派失效 | 成立：和 §3.8「不回部分結果」矛盾 | §3.6 改成三種整批失敗 |
| `include` 區塊可能旁路洩漏 | 成立 | §3.8 規定 `recent_notes` 先過濾再計數、`recent_events` 遮蔽、`actions` 只列可提案的動作 |
| `source_unverifiable` 沒有落點 | 成立：它是 02 propose 的錯誤，03 的讀取工具不回 | §3.7 寫明歸屬，對照列進回饋 02 |
| 用法 D「沒有 execution」不一定是等核准 | 成立 | §2.5 依 state 和 execution 分開處理，只有 `succeeded` 才算已建立 |
| §3.2 還寫「第一版只有 issue_field」 | 成立 | 改成兩種 kind，擴充清單拿掉 `note` |
| `citations` 和 `fields` 的關係沒定 | 成立 | §3.6：`citations` 必須是 `fields` 的子集合 |
| 逐筆狀態的 `next_actions` 要用陣列並有範例 | 成立 | §3.6 `verify_citations` 範例補上 |
| 「撤銷後立刻失敗」要收斂邊界 | 成立 | §3.8、§3.9 改成「撤銷 commit 之後開始的授權檢查」，等待中的搜尋讀取前重驗 |
| 「各工具回應位元組相同」要按工具分 | 成立 | §3.9 依工具分開寫驗收 |
| 回饋 02 四項 | 核對後採用，另加兩項 | 見 §8.2 |

## 附錄 B · 實驗檔案

見 [`experiments/sd/README.md`](../../experiments/sd/README.md)。

主要結果檔：

- `results/frontier.txt`、`results/cic_frontier.txt`：長交易對 frontier 的影響。
- `results/est.txt`：單一條件的估計誤差。
- `results/q3.tsv`、`results/explain/`：多條件查詢的計時與計畫。
- `results/stats_try*.txt`：統計修正的嘗試。
- `results/retype*.txt`：改型別。
- `results/tantivy-probe.txt`：tantivy API 的實際行為。
