# 07 程式品質分析與品質門檻建議

日期：2026-11-13
範圍：`F:\work\oxledger` 本機 `main`（HEAD `git rev-list --count` = 48 個 commit，2026-11-06 至 2026-11-13）
性質：唯讀分析。沒有改任何既有檔案，沒有安裝任何工具；暫存檔都在 `C:\tmp\oxq\` 與 `C:\tmp\oxq-target\`。

---

## 1. 結論摘要

- 整體健康度比檔案大小看起來好：非測試程式碼裡 `unsafe` 為 0、`unwrap` 23、`expect` 39、`panic!` 類 21，TS `strict: true` 而且 `any` 為 0；測試行數與產品行數約 1:1。
- 真正的風險集中在**少數超大函式**與**超大模組**：8 個函式超過 400 行、估計分支數超過 85；17 個產品檔案超過 1,500 行。這些函式同時是 mutation 驗證最難做完整的地方。
- SQL 全部走執行期字串（`sqlx::query*(` 1,097 處，`query!` 巨集 0 處），正確性完全靠整合測試覆蓋到；但目前**沒有覆蓋率量測**，無法知道哪些 SQL 路徑沒被執行過。
- 品質規則沒有寫進 repo：沒有 `[workspace.lints]`、沒有 `clippy.toml`、沒有 CI 設定（沒有 `.github/`），四種 feature 組合的 clippy 與各測試靠 playbook 與 session 紀律執行。
- mutation 驗證目前是人工（各 checklist 記錄「拿掉 X 測試會失敗」），`cargo-mutants --in-diff` 可以把它自動化、而且可以只跑變更範圍。

---

## 2. 分析方法

| 項目 | 工具 | 做法 |
|---|---|---|
| 嚴格 lint | clippy 0.1.99（rustc 1.99.1） | `cargo clippy --offline --workspace --all-targets --all-features --message-format=json -- -W clippy::pedantic -W clippy::nursery`，`CARGO_TARGET_DIR=C:\tmp\oxq-target`。輸出 JSON 用 python 去重後統計 |
| 檔案行數、函式長度、估計複雜度、unwrap 等 | python 自寫腳本（放在 repo 外） | 先把字串與註解換成空白，再用大括號配對找 `fn` 本體。`#[cfg(test)] mod` 與 `tests/`、`test_support.rs` 算測試程式碼 |
| 重複程式碼 | 同一支腳本 | 正規化空白後，以連續 8 個非空、非單獨括號的行為一個視窗，算 SHA-1；同一雜湊出現 2 次以上算一組重複 |
| churn | git | `git log --no-merges --name-only` 算每個檔案被幾個 commit 改過；`--numstat` 算增刪行數 |
| 依賴 | cargo tree、Cargo.lock | `cargo tree --offline -e normal,build --prefix none`（預設與 `--all-features`）；重複版本直接從 `Cargo.lock` 數 |
| 前端 | wc、grep | `web/node_modules` 不存在，所以 svelte-check、vitest 都**沒跑**；只做靜態統計 |

「估計複雜度」是粗略代理指標：`1 + if/while/for/loop/&&/||/=>/?` 的出現次數。它會高估大型 `match` 與大量 `?` 的函式（例如 `error.rs::as_str` 得到 73，其實只是對照表），只能用來排序，不能拿來跟其他工具的 cyclomatic／cognitive complexity 數字直接比。

---

## 3. 量測結果

### 3.1 規模與測試比例（Rust）

| 指標 | 數值 |
|---|---|
| `.rs` 檔案數 | 143 |
| 總行數 | 149,989 |
| 產品程式碼（非空行） | 72,870（ox-core 790、oxledger 72,073） |
| 測試程式碼（非空行） | 70,878（`tests/` 與 `test_support.rs` 67,576；inline `#[cfg(test)]` 3,302） |
| 測試：產品 | 約 0.97 : 1 |
| 測試屬性 | `#[tokio::test]` 397、`#[test]` 121；`tests/` 下 70 個檔案 |
| 產品函式數 | 1,981 |

### 3.2 函式長度分布（產品程式碼）

| 指標 | 數值 |
|---|---|
| 中位數 | 14 行 |
| P90 | 63 行 |
| P99 | 227 行 |
| 最大 | 1,456 行（`openapi.rs::session_routes`，是路由文件表，邏輯很少） |
| > 50 行 | 271 個 |
| > 100 行 | 91 個（clippy `too_many_lines` 在 src 報 87 個） |
| > 200 行 | 26 個 |
| 估計複雜度 > 25 | 93 個 |
| 估計複雜度 > 50 | 25 個 |
| 大括號巢狀 > 6 層 | 32 個 |

### 3.3 最大的函式（排除純資料表）

| 函式 | 位置 | 行數 | 估計複雜度 | 巢狀 |
|---|---|---|---|---|
| `search_with` | `crates/oxledger/src/service.rs:1267` | 673 | 168 | 8 |
| `run_ops` | `crates/oxledger/src/commands.rs:3764` | 672 | 116 | 8 |
| `parse_seen` | `crates/oxledger/src/commands.rs:1504` | 579 | 153 | 7 |
| `plan` | `crates/oxledger/src/time_entries.rs:696` | 566 | 147 | 7 |
| `execute` | `crates/oxledger/src/views.rs:3462` | 483 | 90 | 7 |
| `simulate` | `crates/oxledger/src/commands.rs:3217` | 466 | 124 | 8 |
| `run_loop` | `crates/oxledger/src/ai/runner.rs:740` | 441 | 97 | 8 |
| `apply` | `crates/oxledger/src/undo.rs:975` | 425 | 86 | 6 |
| `seed_demo` | `crates/oxledger/src/seed.rs:43` | 346 | 65 | 3 |
| `agent_write_tx` | `crates/oxledger/src/changesets.rs:1409` | 336 | 87 | 4 |
| `build_preview` | `crates/oxledger/src/undo.rs:497` | 289 | 58 | 6 |
| `parse_shape` | `crates/oxledger/src/views.rs:742` | 289 | 87 | 9 |
| `apply_tx` | `crates/oxledger/src/changesets.rs:4182` | 262 | 50 | 3 |
| `resolve_cond` | `crates/oxledger/src/filter/v2.rs:888` | 260 | 83 | 7 |
| `create` | `crates/oxledger/src/ai_team.rs:1072` | 256 | 61 | 4 |

資料表型的長函式（邏輯少、長度高）：`openapi.rs::session_routes` 1,456 行、`api.rs::base_router` 292 行、`registry.rs::tools` 262 行。這些不是複雜度風險，但它們是 churn 熱點（見 3.6）。

### 3.4 最大的產品檔案（非空行）

| 檔案 | 產品行數 | inline 測試 | clippy 嚴格警告 |
|---|---|---|---|
| `src/commands.rs` | 5,026 | 73 | 106 |
| `src/changesets.rs` | 4,756 | 64 | 97 |
| `src/views.rs` | 3,997 | 54 | 95 |
| `src/forms.rs` | 3,720 | 0 | 67 |
| `src/api.rs` | 3,482 | 0 | — |
| `src/versions.rs` | 2,730 | 218 | 51 |
| `src/service.rs` | 2,391 | 0 | 105 |
| `src/notifications.rs` | 2,183 | 0 | 50 |
| `src/agents.rs` | 2,157 | 0 | 50 |
| `src/filter/v2.rs` | 2,156 | 127 | 85 |
| `src/search.rs` | 2,113 | 555 | 109 |
| `src/ai_team.rs` | 2,087 | 0 | — |
| `src/undo.rs` | 2,029 | 0 | — |

（路徑都在 `crates/oxledger/` 下。）超過 1,500 個產品行的檔案共 17 個。任務說明提到的 `review_queue.rs` 其實只有 778 行，不在熱點內。

### 3.5 風險樣式（非測試程式碼）

| 樣式 | 數量 | 主要位置 |
|---|---|---|
| `unsafe` | 0 | — |
| `.unwrap()` | 23 | `filter/v2.rs` 5、`api.rs` 4（登入限流的 `Mutex::lock().unwrap()`）、`changesets.rs` 3、`github/http.rs` 3 |
| `.expect(` | 39 | `commands.rs` 12、`changesets.rs` 5 |
| `panic!`／`unreachable!`／`todo!`／`unimplemented!` | 21 | `commands.rs` 8、`filter.rs` 3 |
| `as` 數值轉型 | 194 | `changesets.rs` 29、`service.rs` 21、`ai/runner.rs` 13 |
| `.clone()` | 616 | `views.rs` 82、`forms.rs` 61 |
| `#[allow(...)]` | 17 | 15 個 `too_many_arguments`、1 個 `enum_variant_names`、`undo.rs` 整個模組 `type_complexity` |
| `TODO`／`FIXME`／`HACK` | 0（唯一命中是測試裡的字串 `"XXX{sign}..."`） | — |
| 執行期 SQL `sqlx::query*(` | 1,097 處 | 編譯期檢查的 `query!` 巨集 0 處 |

### 3.6 Clippy pedantic + nursery（只統計，不修改）

去重鍵：(lint, 檔案, 行, 欄)。同一位置在 lib 與 test target 各報一次的只算一次。現有預設 lint 組合在專案裡已經是 `-D warnings` 乾淨的，以下全部來自 pedantic／nursery。

- 不重複警告 2,820 個（src 1,960、`tests/` 860），71 種 lint，全部是 warning。
- 和正確性比較有關的：

| lint | src | tests | 意義 |
|---|---|---|---|
| `too_many_lines` | 87 | 165 | 函式超過 100 行 |
| `cast_possible_wrap` | 54 | 7 | 無號轉有號可能變負數 |
| `cast_possible_truncation` | 49 | 22 | 大轉小可能截斷 |
| `cast_sign_loss` | 32 | 4 | 有號轉無號丟掉負號 |
| `cast_precision_loss` | 11 | 11 | 整數轉浮點失準 |
| `match_same_arms` | 26 | 0 | 不同分支內容相同，可能是複製貼上 |
| `missing_panics_doc` | 17 | 0 | 會 panic 卻沒寫在文件 |
| `redundant_clone` | 13 | 8 | 多餘的 clone |
| `needless_pass_by_value` | 10 | 43 | — |
| `significant_drop_tightening` | 8 | 4 | 鎖的持有範圍可以更短（`api.rs:711/722/731` 的登入限流、`search.rs` 的 writer 鎖） |
| `option_option` | 4 | 0 | `Option<Option<T>>`（`commands.rs:865/955`、`time_entries.rs:167`、`views.rs:3468`），通常是 PATCH「沒給／設成 null」語意，可考慮換成具名 enum |

- 風格類（佔大宗，不建議全面修）：`missing_errors_doc` 366、`doc_markdown` 356、`use_self` 344、`many_single_char_names` 243（幾乎都在測試）、`must_use_candidate` 163、`missing_const_for_fn` 94。
- 警告最多的檔案：`search.rs` 109、`commands.rs` 106、`service.rs` 105、`changesets.rs` 97、`ox-core/src/filter.rs` 95（其中 `use_self` 68）、`views.rs` 95、`filter/v2.rs` 85。

### 3.7 重複程式碼跡象

8 行視窗重複組數：361 組。

| 檔案（重複視窗數） | | 跨檔重複（組數） | |
|---|---|---|---|
| `openapi.rs` | 162 | `service.rs` ↔ `views.rs` | 19 |
| `versions.rs` | 87 | `changesets.rs` ↔ `undo.rs` | 11 |
| `commands.rs` | 66 | `changesets.rs` ↔ `versions.rs` | 9 |
| `views.rs` | 55 | `project_tabs.rs` ↔ `views.rs` | 9 |
| `registry.rs` | 50 | | |
| `changesets.rs` | 44 | | |

`openapi.rs` 與 `registry.rs` 的重複多半是宣告式表格（可接受，或改用巨集／建構函式）。值得看的是跨檔重複：`service.rs`↔`views.rs`（查詢組裝）、`changesets.rs`↔`undo.rs`／`versions.rs`（寫入與回溯路徑），這種重複最容易出現「修了一邊忘了另一邊」。

### 3.8 Churn（48 個 commit，不含 merge）

| 檔案 | 被幾個 commit 改過 | 增刪行數合計 |
|---|---|---|
| `src/api.rs` | 22 | 3,963 |
| `src/lib.rs` | 21 | — |
| `src/service.rs` | 19 | 3,489 |
| `src/openapi.rs` | 18 | — |
| `src/mcp.rs` | 15 | — |
| `src/error.rs` | 15 | — |
| `src/main.rs` | 12 | — |
| `src/filter.rs` | 12 | — |
| `src/commands.rs` | 10 | 5,600 |
| `src/changesets.rs` | 10 | 5,518 |

歷史只有一週、多數 commit 是一整個階段，所以「被幾個 commit 改過」比增刪行數更有意義。`api.rs`、`lib.rs`、`openapi.rs`、`mcp.rs`、`error.rs` 是每個功能都要碰的接線檔，平行 session 時最容易衝突。

### 3.9 依賴

| 指標 | 數值 |
|---|---|
| 直接依賴（oxledger，all-features，normal） | 31 |
| 解析後的套件（預設 features，normal+build） | 279 |
| 解析後的套件（all-features） | 290 |
| `Cargo.lock` 項目 | 334（317 個不同 crate） |
| 來源 | 全部 crates.io registry（332），沒有 git 依賴 |
| 有多個版本的 crate | 14 個 |

重複版本：`digest` 0.10／0.11、`sha2` 0.10／0.11、`block-buffer`、`crypto-common`、`cpufeatures`、`chacha20` 0.9／0.10（直接依賴 `sha2 0.11`、`hmac 0.13`，而 `argon2 0.5`、`chacha20poly1305 0.10` 還在舊的 RustCrypto 世代）；`rand` 0.9／0.10、`rand_core` ×3、`getrandom` ×3（`rmcp` 與 `sqlx-postgres` 拉 `rand 0.10`，專案直接用 `rand 0.9`）；`syn` 2／3（`async-trait` 經 `rmcp`）；`hashbrown`、`base64` 0.22／0.23、`windows-sys` ×3、`r-efi`。這些都是生態系世代交替造成的，不是錯誤，但會增加編譯時間與稽核面。

### 3.10 前端（`web/`）

| 指標 | 數值 |
|---|---|
| `.svelte` | 31 個、4,067 行 |
| `.ts`（src） | 20 個、1,662 行 |
| 單元測試 | 1 個檔案（`src/lib/lib.test.ts`，322 行、約 35 個案例） |
| E2E | `e2e/flows.spec.ts` 426 行、17 個 `test(`；加上 fixture／agent 共 632 行 |
| 超過 200 行的元件 | 8 個：`routes/cs/[id]/+page.svelte` 441、`routes/issue/[key]/+page.svelte` 407、`routes/project/[key]/settings/+page.svelte` 309、`routes/agents/[id]/activity` 285、`routes/agents` 247、`routes/review` 230、`routes/agents/new` 228、`routes/+page.svelte` 206 |
| TypeScript | `strict: true`、`checkJs: true` |
| `any`／`as any` | 0 |
| `@ts-ignore`／`eslint-disable`／`svelte-ignore` | 0 |
| `{@html` | 0 |
| ESLint | 沒有設定 |

前端規模小、型別紀律很好；缺的是 lint 與元件層的單元測試（大頁面的邏輯目前只靠 E2E）。

---

## 4. 品質熱點前 10 名

依「出錯機率 × 影響 × 驗證難度」排序。

| # | 熱點 | 證據 | 建議做法 |
|---|---|---|---|
| 1 | **超大核心函式** | `service.rs::search_with` 673 行／估計複雜度 168；`commands.rs::run_ops` 672／116、`parse_seen` 579／153、`simulate` 466／124；`time_entries.rs::plan` 566／147。26 個函式 > 200 行 | 修改時順手拆（擷取「解析／檢查／執行」等子步驟為具名函式），不做大爆炸重構；加 `too_many_lines` 的棘輪（數量只能減不能增） |
| 2 | **SQL 沒有編譯期檢查，也沒有覆蓋率** | `sqlx::query*(` 1,097 處、`query!` 0 處；沒有任何覆蓋率工具 | 先用 `cargo-llvm-cov` 量出哪些 SQL 所在函式從沒被執行；之後再決定（A4）是否對靜態 SQL 改用 `query!` |
| 3 | **mutation 驗證是人工的** | checklist 裡大量「拿掉 X、測試失敗、已還原」（如 `docs/impl/m1-checklist.md:597`、`p2-checklist.md:36` 9 個存活 mutation）；`p1-backend-gaps.md` G25 是存活 mutation 才發現的缺口 | 每階段用 `cargo-mutants --in-diff` 只跑變更部分；定期全量跑。人工 mutation 保留給跨模組語意（鎖順序、交易） |
| 4 | **超大模組** | 17 個產品檔案 > 1,500 行；`commands.rs` 5,026、`changesets.rs` 4,756、`views.rs` 3,997、`forms.rs` 3,720、`api.rs` 3,482 | 依現有段落改成目錄模組（`commands/{parse,simulate,run}.rs` 等），只搬不改；以「檔案行數不准再長」為棘輪 |
| 5 | **接線檔 churn 與衝突面** | `api.rs` 22 次、`lib.rs` 21、`service.rs` 19、`openapi.rs` 18、`mcp.rs`／`error.rs` 15；`api.rs::base_router` 292 行、`registry.rs::tools` 262 行 | 讓各功能模組自己回傳 `Router`／路由文件片段，`base_router` 只做合併；平行 session 衝突會變少 |
| 6 | **數值轉型** | src 內 `cast_possible_wrap` 54、`cast_possible_truncation` 49、`cast_sign_loss` 32；`as` 共 194 處，`changesets.rs` 29、`service.rs` 21 | 對工時、金額、計數類路徑把這 4 個 cast lint 設成 warn（只看新程式碼），改用 `try_from` 或明確的飽和運算 |
| 7 | **跨檔重複的寫入／查詢邏輯** | `service.rs`↔`views.rs` 19 組、`changesets.rs`↔`undo.rs` 11 組、`changesets.rs`↔`versions.rs` 9 組 | 逐組看是否為同一規則的兩份實作；是的話抽共用函式（同 `ox-core::Scope::predicate()` 的「規則只有一份」原則） |
| 8 | **參數過多與型別過深** | 15 個 `#[allow(clippy::too_many_arguments)]`（`changesets.rs` 4、`undo.rs` 2…）、`undo.rs:18` 整模組 `#![allow(clippy::type_complexity)]` | 改成參數 struct／type alias；把 `allow` 改成 `#[expect(..., reason = "...")]`，讓不再需要時會提醒 |
| 9 | **品質規則不在 repo 裡** | 沒有 `[workspace.lints]`、`clippy.toml`、`.github/`；四種 feature 組合靠 playbook；`s6b_github_sync` 有 `#![cfg(feature = "github")]`，全套測試會靜默略過（`docs/playbook.md:207`） | 加一支 `scripts/quality` 腳本把現有檢查全列出來，並在預設 `cargo test` 時印出被 feature 略過的測試檔（A2、A6） |
| 10 | **前端缺 lint 與元件測試** | ESLint 0 設定；單元測試只有 1 個檔案；8 個頁面 > 200 行，最大 441 行 | 導入 ESLint flat config + `eslint-plugin-svelte` + `typescript-eslint`；大頁面把資料轉換邏輯抽到 `.ts` 再用 vitest 測 |

次要觀察（不列前 10）：`significant_drop_tightening` 8 處，多是 `std::sync::Mutex` 鎖住 HashMap 的短函式，實際影響小；`option_option` 4 處語意可讀性；依賴重複版本 14 個（等上游）。

---

## 5. 工具比較（只研究，未安裝）

| 工具 | 用途 | 維護者 | 所在地 | 授權 | 要求 | 適合時機 |
|---|---|---|---|---|---|---|
| clippy（pedantic／nursery／restriction 個別 lint） | lint | Rust 專案（rust-lang） | 國際社群 | MIT/Apache-2.0 | 已安裝 | 每階段；用 `[workspace.lints]` 個別開 |
| `cargo-llvm-cov` | 覆蓋率（LLVM source-based），支援 `--fail-under-lines`／`--fail-under-regions`／`--fail-under-functions`、輸出 lcov | taiki-e（個人，Taiki Endo） | 日本（依公開個人資料，未逐一查證） | MIT/Apache-2.0 | 需 rustup 元件 `llvm-tools-preview`（下載） | 合流；定期看趨勢 |
| `cargo-mutants` | mutation testing；`--in-diff` 只測 diff 範圍；支援 `cargo nextest` | sourcefrog（個人，Martin Pool） | 未查證（非中國） | MIT | 每個 mutant 都要跑測試，整合測試要 PostgreSQL | 每階段 `--in-diff`；定期全量 |
| `cargo-deny` | advisories、licenses、bans（含重複版本偵測）、sources | Embark Studios | 瑞典 | MIT/Apache-2.0 | 讀 RustSec 資料庫（要網路） | 合流；定期（新漏洞公告不需要改程式就會出現） |
| `cargo-audit` | RustSec 漏洞稽核，另有 `cargo audit bin` | RustSec 組織（Rust Secure Code WG） | 國際社群 | MIT/Apache-2.0 | 同上 | 與 cargo-deny 擇一（deny 已涵蓋 advisories） |
| `cargo-machete` | 未使用依賴，快但不精確（文字掃描），可用 `--with-metadata` 降低誤報 | bnjbvr（個人，Benjamin Bouvier） | 法國（依公開個人資料） | MIT | stable | 定期 |
| `cargo-udeps` | 未使用依賴，靠編譯結果，較準 | est31（個人） | 未查證（非中國） | MIT/Apache-2.0 | 需要 nightly | 定期；若不想裝 nightly 就只用 machete |
| `rust-code-analysis` | 複雜度指標（多種 maintainability metrics） | Mozilla | 美國 | MPL-2.0 | 另行安裝 | 可選；本次用自寫腳本與 clippy 已夠排序 |
| SonarQube（Server／Cloud） | 品質平台：Rust 分析（可匯入 clippy JSON、cognitive complexity、重複、覆蓋率 lcov/cobertura）；預設 quality gate「Sonar way」只看新程式碼 | SonarSource SA | 瑞士 | Community Build 為 LGPL；商業版付費 | 要架伺服器或用雲端（雲端會把原始碼送出去） | 可選；**公司資料規則**下需確認能否用 |
| ESLint | JS/TS lint | OpenJS Foundation 旗下 ESLint 團隊 | 美國（基金會） | MIT | npm 安裝 | 每階段 |
| `eslint-plugin-svelte` | Svelte 5（含 runes、`.svelte.ts`）lint，支援 flat config | sveltejs 組織 | 國際社群 | MIT | ESLint ≥ 8.57.1 或 9 | 每階段 |
| `typescript-eslint` | TS 型別感知 lint | typescript-eslint 開源組織 | 國際社群 | MIT/BSD-2 | npm | 每階段 |
| `svelte-check`（已有） | Svelte／TS 型別檢查 | sveltejs | 國際社群 | MIT | 已在 devDependencies | 每階段 |

都不是中國產品或中國維護者。標「未查證」的是本次沒有找到可直接引用的所在地資料，只確認不是中國。

---

## 6. 建議的品質門檻分層

原則：每階段要快（幾分鐘內），合流要完整，定期跑貴的或「不改程式也會變」的。數量型指標一律用**棘輪**（記下基準值，只准持平或下降），不設一次就要達到的絕對門檻，避免一開始就要大改。

### 6.1 每個階段（實作 session 結束、獨立驗證前）

| 檢查 | 現況 | 備註 |
|---|---|---|
| `cargo fmt --check` | 已有 | |
| `cargo clippy -D warnings`（4 種 feature 組合） | 已有 | |
| 選定的 pedantic lint（`too_many_lines`、4 個 `cast_*`、`match_same_arms`、`redundant_clone`）以 warn 計數，**不准增加** | 新 | 用 `--message-format=json` 計數，跟基準檔比 |
| 受影響的 `cargo test --test <suite>` | 已有 | |
| `cargo-mutants --in-diff`（對 `git diff main...HEAD`） | 新，取代大部分人工 mutation | 需安裝；存活 mutant 由驗證子代理判斷是等價還是缺口 |
| 檔案行數與函式長度棘輪（本次的 python 腳本） | 新 | 變更的檔案不准變更大於門檻（例如 > 1,500 行的檔案不准再長） |
| `svelte-check`、`vitest` | 已有 | |
| ESLint（`eslint-plugin-svelte` + `typescript-eslint`） | 新 | 需安裝 |

### 6.2 合流（merge 進 `main`）

| 檢查 | 備註 |
|---|---|
| 完整 `cargo test`，含 `--features test-support,github` 的 `s6b_github_sync` | 避免 feature 閘門讓測試被靜默略過 |
| Playwright E2E | 已有 |
| `cargo llvm-cov`：記錄總覆蓋率，並看 diff 覆蓋率（新程式碼行） | 先只記錄不擋；有基準後再決定門檻（A5） |
| `cargo deny check`（advisories、licenses、bans、sources） | GPL-2.0-or-later 專案要確認依賴授權相容 |
| 嚴格 clippy 總數、重複視窗數、> 100 行函式數的棘輪 | 寫進一個 TSV 趨勢檔 |

### 6.3 定期（每週或每個里程碑）

| 檢查 | 備註 |
|---|---|
| `cargo-mutants` 全量（可用 `--shard` 分片） | 1,981 個產品函式、整合測試要連 PG，估計要數小時；本次沒有實測 |
| `cargo deny check advisories`（或 `cargo audit`） | 新公告不需要改程式就會出現 |
| `cargo-machete`（必要時 `cargo +nightly udeps`） | 未使用依賴 |
| 依賴升級與重複版本檢視 | 目前 14 個 crate 有多版本 |
| 指標趨勢快照（本報告第 3 節的數字） | 追加到 TSV，畫趨勢 |

---

## 7. 要使用者決定的 A 類問題

### A1. 要不要安裝品質工具？裝哪些？

| 選項 | 內容 | 代價 |
|---|---|---|
| a. 分批（建議） | 第一批：`cargo-deny`、`cargo-llvm-cov`（含 rustup 元件 `llvm-tools-preview`）、`cargo-mutants`；第二批：前端 `eslint`、`eslint-plugin-svelte`、`typescript-eslint`；之後視需要 `cargo-machete` | 需 `cargo install` 與 `npm install`（都要下載）；第一次編譯工具數分鐘 |
| b. 全部一次裝 | 包含 nightly 與 `cargo-udeps` | 多一條 nightly toolchain 要維護 |
| c. 都不裝 | 只用本次的 python 腳本與 clippy | mutation 繼續人工、沒有覆蓋率與漏洞稽核 |

建議 a：mutation 與覆蓋率是目前最缺的兩塊，cargo-deny 成本最低。

### A2. 要不要把 lint 政策寫進 repo？

| 選項 | 內容 | 代價 |
|---|---|---|
| a. 只開挑過的 lint（建議） | 根 `Cargo.toml` 加 `[workspace.lints.clippy]`：`too_many_lines`、`cast_possible_truncation`、`cast_possible_wrap`、`cast_sign_loss`、`match_same_arms`、`redundant_clone` 設 warn；`clippy.toml` 設門檻；`allow` 改 `expect` 並寫 reason | 既有 1 百多個警告要用棘輪容忍，`-D warnings` 時要另外處理（例如這些 lint 只在計數腳本裡打開） |
| b. 整組 pedantic | 一次開 | 2,820 個警告，八成是文件與風格類，修起來成本高、收益低 |
| c. 維持現狀 | 不動 | 規則只在 playbook，新 session 容易漏 |

### A3. 超大檔案與函式怎麼處理？

| 選項 | 內容 | 代價 |
|---|---|---|
| a. 碰到才拆 + 棘輪（建議） | 改到某函式時順便拆；長度與檔案大小只准減 | 進度慢，但不打斷功能開發，風險最小 |
| b. 排一個專門重構階段 | 先拆前 3 名（`search_with`、`run_ops`／`parse_seen`／`simulate`、`time_entries::plan`） | 佔一個階段；需要完整測試與 mutation 驗證保護 |
| c. 不處理 | | 這些函式會持續是 mutation 存活與缺陷的來源 |

### A4. SQL 的正確性怎麼保證？

| 選項 | 內容 | 代價 |
|---|---|---|
| a. 先量覆蓋率再決定（建議） | 用 llvm-cov 找出沒被執行的 SQL 所在函式，補測試 | 依賴 A1 安裝 llvm-cov |
| b. 靜態 SQL 改用 `sqlx::query!` | 編譯期對 schema 檢查；用 `cargo sqlx prepare` 產生離線快取 `.sqlx/` | 要裝 `sqlx-cli`；編譯需要 DB 或離線快取；動態組裝的 SQL（`db::push_pred`）無法套用 |
| c. 維持現狀 | 靠整合測試 | 不知道哪些路徑沒被測到 |

### A5. 覆蓋率門檻設多少？

| 選項 | 內容 | 代價 |
|---|---|---|
| a. 先記錄基準，再對新程式碼設 diff 覆蓋率（建議） | 參考 SonarSource「Sonar way」：新程式碼 ≥ 80%、重複 ≤ 3%，小變更（< 20 行）不判定 | 要寫 diff 覆蓋率比對腳本 |
| b. 全專案 `--fail-under-lines N` | 簡單 | 數字容易被灌（測試跑過但沒斷言），也會擋住和本次變更無關的合流 |
| c. 只看不擋 | | 沒有約束力 |

### A6. 品質檢查在哪裡跑？

| 選項 | 內容 | 代價 |
|---|---|---|
| a. 本機腳本（建議先做） | `scripts/quality.ps1`（或 python）跑 6.1／6.2 的檢查，結果寫 TSV | 靠 session 記得跑 |
| b. GitHub Actions | repo 在 GitHub 上 | 需要 PostgreSQL service container；要確認此專案資料是否允許放到外部 CI（公司資料規則） |
| c. 自架 SonarQube | 有趨勢頁與 quality gate | 要維護一台伺服器 |

### A7. mutation testing 放在哪一層？

| 選項 | 內容 | 代價 |
|---|---|---|
| a. 每階段 `--in-diff` + 定期全量（建議） | 驗證子代理拿 cargo-mutants 結果當起點，再補人工跨模組 mutation | 每階段多幾分鐘到幾十分鐘（視變更範圍與測試時間） |
| b. 只在合流跑 | | 存活 mutant 發現得晚 |
| c. 維持人工 | | 覆蓋面取決於驗證者 |

---

## 8. 研究過程

### 8.1 執行的指令

```sh
# 工具版本（Git Bash，cargo 不在 PowerShell PATH 裡）
export PATH="$HOME/.cargo/bin:$PATH"
cargo --version; cargo clippy --version; rustc --version
# cargo 1.99.1、clippy 0.1.99、rustc 1.99.1

# 嚴格 clippy（4 分 09 秒，全新 target）
CARGO_TARGET_DIR='C:\tmp\oxq-target' cargo clippy --offline --workspace --all-targets --all-features \
  --message-format=json -- -W clippy::pedantic -W clippy::nursery > C:/tmp/oxq/clippy-strict.json

# 依賴
cargo tree --offline -e normal,build --prefix none [--all-features] | sort -u | wc -l
cargo tree --offline --all-features -d -e normal,build --depth 0
cargo tree --offline --all-features -i rand@0.10.3 -e normal

# churn
git log --no-merges --name-only --format= -- crates web/src | sort | uniq -c | sort -rn
git log --no-merges --numstat --format= -- crates web/src

# 其他
grep -rhoE "sqlx::query(_as|_scalar)?(::<[^>]*>)?\(" crates/*/src | wc -l
grep -rnE ":\s*any\b|<any>|as any\b|any\[\]" web/src
```

python 腳本（`rsmetrics.py`、`clippystat.py`）放在 session 的暫存目錄，輸出在 `C:\tmp\oxq\rsmetrics.json`、`C:\tmp\oxq\clippy-stat.json`。

### 8.2 來源

- Clippy lint 分組與預設等級、`restriction` 不應整組開：<https://doc.rust-lang.org/clippy/>
- Clippy 設定檔（`too-many-lines-threshold` 等）：<https://doc.rust-lang.org/clippy/configuration.html>；門檻預設值（100、25、7）來自 lint 設定表，本次未逐一開頁確認
- `cognitive_complexity` 的分組歷史（nursery → restriction 的討論）：<https://github.com/rust-lang/rust-clippy/issues/5418>
- Cargo `[workspace.lints]`（1.74 起）：<https://doc.rust-lang.org/cargo/reference/workspaces.html>
- rustc `-C instrument-coverage` 與 `llvm-tools-preview`：<https://doc.rust-lang.org/rustc/instrument-coverage.html>
- cargo-llvm-cov（`--fail-under-*`）：<https://github.com/taiki-e/cargo-llvm-cov>
- cargo-mutants：<https://mutants.rs/>、`--in-diff`：<https://mutants.rs/in-diff.html>、repo：<https://github.com/sourcefrog/cargo-mutants>
- cargo-deny：<https://github.com/EmbarkStudios/cargo-deny>
- cargo-audit：<https://github.com/rustsec/rustsec/tree/main/cargo-audit>
- cargo-machete：<https://github.com/bnjbvr/cargo-machete>
- cargo-udeps：<https://github.com/est31/cargo-udeps>
- rust-code-analysis：<https://github.com/mozilla/rust-code-analysis>
- eslint-plugin-svelte：<https://github.com/sveltejs/eslint-plugin-svelte>
- SonarQube quality gate（Sonar way 條件）：<https://docs.sonarsource.com/sonarqube-server/latest/quality-standards-administration/managing-quality-gates/introduction-to-quality-gates/>
- SonarQube Rust 支援（clippy 匯入、cognitive complexity、覆蓋率匯入）：<https://docs.sonarsource.com/sonarqube-server/analyzing-source-code/languages/rust>、<https://docs.sonarsource.com/sonarqube-cloud/advanced-setup/languages/rust>

### 8.3 踩到的坑

| 症狀 | 原因 | 處理 |
|---|---|---|
| PowerShell 找不到 `cargo` | `~/.cargo/bin` 不在這個 shell 的 PATH | 改用 Git Bash 並 `export PATH="$HOME/.cargo/bin:$PATH"` |
| clippy 警告數量翻倍 | `--all-targets` 時同一個 src 位置在 lib 與 test target 各報一次 | 以 (lint, 檔案, 行, 欄) 去重 |
| `cargo tree -d` 同一版本列兩次 | 不同 feature 組合下的同一版本 | 改從 `Cargo.lock` 數不同版本 |
| python `-c` 內 `'\\'` 在 bash 雙引號裡變成未結束字串 | 跳脫層次 | 寫成 heredoc 檔再執行，用 `chr(92)` |
| 「估計複雜度」把 `error.rs::as_str` 算成 73 | 代理指標把 `=>` 每個 match arm 都算一次 | 報告中標明只能排序；資料表型函式另列 |
| 跑 clippy 時看到其他 `cargo`／`rustc` 程序 | 其他 session 同時在編譯 | 用獨立 `CARGO_TARGET_DIR` 避免搶鎖 |
| 「`review_queue.rs` 很大」的前提不成立 | 實際 778 行 | 以實測為準 |

### 8.4 沒做的事

- 沒跑 `cargo test`、svelte-check、vitest、Playwright（`web/node_modules` 不存在；測試要 PostgreSQL）。
- 沒量覆蓋率與 mutation 分數（工具未安裝）；第 6.3 節的「全量 mutation 要數小時」是推估，沒有實測。
- 工具維護者所在地標「未查證」的，沒有找到可直接引用的資料。
