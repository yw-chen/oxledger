# 09 Golden file 測試：工具選擇、適用範圍與業界做法

日期：2026-11-14　範圍：後端整合測試（crates/oxledger/tests）、前端 vitest 與 Playwright
性質：研究報告。沒有改任何程式、沒有安裝任何東西、沒有跑任何測試。
前置文件：05-testing-speed.md、08-review-05-06-07.md（D1–D14 已定案：nextest、slot DB＋TRUNCATE、只測自己改的部分、合流才跑全套等）。

---

## 1. 摘要

1. **後端用 insta＋cargo-insta。** 維護者是 Armin Ronacher（奧地利）和 Maximilian Roos（美國洛杉磯），2026-10-03 發布 1.49.0，有 redaction、regex filter、glob、inline snapshot，`cargo insta test --test-runner nextest` 可以直接接 nextest。CI（`CI=true`）時不寫入任何快照檔，可再加 `--unreferenced=reject` 擋孤兒快照。另一個常被比較的 expect-test 已在 **2026-11-10 封存**（併回 rust-analyzer 主 repo），不要選。
2. **不要把 UUID、時間「一律換成 `[uuid]`」。** 本專案的回應裡 ID 會互相參照（issue → project → user），全部抹成同一個字串會讓「指錯人」的 bug 通過。建議在 `tests/common` 寫一個正規化函式：依出現順序把 ID 換成穩定標籤（`<issue#1>`、`<user:alice>`），時間換 `<ts>`，xid 換 `<xid>`，浮點數四捨五入，集合排序。之後才交給 insta。
3. **最值得先做的 golden：** Markdown→HTML（m2_1_markdown 目前 217 個手寫斷言，還自己寫了一個 HTML tag parser 來比對）、錯誤碼目錄、migration 後的 schema、送給 AI provider 的請求內容、固定語料的搜尋排序。**不適合：** 執行時間、背景 worker 的時序、整個 API 回應原封不動（會膨脹）、視覺截圖（現階段）。
4. **既有的 OpenAPI golden（`OXLEDGER_UPDATE_API_SPEC=1`）保留。** `api/openapi.json` 和 `api/types.ts` 是前端實際使用的產物，不該變成帶 YAML 標頭的 `.snap`。只補一條：`CI` 有設時忽略更新旗標。
5. **前端：** vitest 純函式用 `toMatchInlineSnapshot`，HTML／長文字用 `toMatchFileSnapshot`；不對整個元件 DOM 拍快照。Playwright（lockfile 是 1.65.0）先導入 `toMatchAriaSnapshot`，存成 `.aria.yml`；`toHaveScreenshot` 先不用，因為只有 Windows 開發機，官方文件明說基準圖要在同一個環境產生，要穩定得用 Linux 容器（要下載映像，需要你同意）。
6. **Golden 只鎖住「已知的輸出」，要搭配：** proptest 驗證不變式（例如 sanitizer 輸出絕不含 `<script>`）、cargo-mutants `--in-diff` 檢查斷言有沒有真的抓到改動、每個等待都要有硬逾時（9 小時卡住事件）。DST（deterministic simulation testing）和 Pact 類 contract testing 對本專案目前成本大於效益。
7. **審查流程是成敗關鍵。** 快照測試最常見的失敗是「不看 diff 就 accept」。規則：`.snap.new`、`.pending-snap` 進 `.gitignore`；合流時 `CI=true cargo insta test --check --unreferenced=reject`；PR 說明要逐一說明每個改動的快照；單一快照超過約 200 行就拆。

---

## 2. 現況（唯讀觀察）

| 項目 | 觀察 |
|---|---|
| 斷言風格 | 幾乎全部是 `assert_eq!`／`assert!(x.contains(..))` 逐欄位比對。`m1_10_ai_run.rs` 逐一檢查請求 header、body 欄位；`p2_search.rs` 比對 frontier、generation 數值 |
| 已有的 golden 機制 | `s2_api.rs::api_spec_is_current`：產生 `api/openapi.json` 和 `api/types.ts`，`OXLEDGER_UPDATE_API_SPEC=1` 時覆寫，比較前把 `\r\n` 換成 `\n` |
| m2_1_markdown.rs | 不在 main，在 `.claude/worktrees/p2-backend-01df05`（2,009 行、217 個 assert）。為了比對 HTML 自己寫了 `Tag` 結構和 `unescape`，XSS 語料逐條寫斷言 |
| nextest 設定 | 在 p2-backend 工作樹的 `.config/nextest.toml`：`retries = 0`、`slow-timeout = { period = "60s", terminate-after = 10 }`、`fail-fast = false`、drop-database 與 horizon 敏感測試的 test group |
| 前端 | 15 個 `*.test.ts`（純函式為主，`expect(..).toBe`）、`e2e/flows.spec.ts` 1,458 行，用 `getByTestId` 加 `toContainText`；vitest 4.1.12、Playwright 1.65.0；E2E `workers: 1`、`timeout: 60_000` |
| CI | repo 沒有 `.github/workflows` 等 CI 設定。下文說的「CI」指合流時在本機跑的全套檢查，靠 `CI=true` 環境變數切換行為 |
| 換行 | `.gitattributes`：`* text=auto eol=lf`；playbook 記過 CRLF 造成 perl 替換失效與 migration checksum 問題 |
| 依賴版本慣例 | 部分依賴用 `=` 釘版本（`comrak = "=0.59.0"`、`ammonia = "=4.2.4"`） |

---

## 3. 發現

### 3.1 Rust golden／snapshot 工具比較

| 工具 | 最新版本（crates.io） | 維護者（GitHub 資料） | 狀態 | 更新方式 | 不確定值 | nextest |
|---|---|---|---|---|---|---|
| **insta**＋cargo-insta | 1.49.0，2026-10-03；下載 1.29 億次 | Armin Ronacher（mitsuhiko，Earendil，**奧地利**）；Maximilian Roos（max-sixty，**美國洛杉磯**）。crates.io owner 就是這兩位；commit 數 623／288 | 活躍（2026-10-26 有 push） | `cargo insta review`（互動 a/r/s）、`accept`、`reject`；`INSTA_UPDATE=auto/always/unseen/new/no`；寫 `.snap.new` 待審 | redaction 選擇器（`.id`、`[]`、`.**`）、`dynamic_redaction`、`sorted_redaction`、`rounded_redaction`、regex `add_filter`、`sort_maps` | `cargo insta test --test-runner nextest`；1.48 新增 `--nextest-profile`；1.49 修好 `--all-targets` 搭 nextest |
| expect-test | 1.5.1，2026-11-10 | rust-analyzer 組織（crates.io owner 是 `rust-lang-owner`）；歷來主要是 matklad（Alex Kladov，里斯本） | **2026-11-10 封存**，README 說改為 vendored 在 rust-analyzer repo | `UPDATE_EXPECT=1` 直接改原始碼 | 沒有 redaction，要自己先正規化 | 可以（只是普通 `#[test]`） |
| snapbox／trycmd | snapbox 1.2.2（2026-05-26）、trycmd 1.2.1（2026-07-21） | Ed Page（epage，**美國德州**，Cargo 團隊）＋ `rust-cli` 組織 | 活躍；Cargo 自己的測試套件在用 | `SNAPSHOTS=overwrite`；Cargo 文件提醒「覆寫後一定要檢查」 | `[..]` 萬用字元、`Redactions`；`json` feature | 可以 |
| goldenfile | 1.11.0，2026-02-25 | Calder Coalson（calder，SpaceX，**美國西雅圖**），單人維護 | 維護中但更新少 | `UPDATE_GOLDENFILES=1` | 沒有 redaction | 可以 |
| （參考）datatest-stable | 0.3.3 | Rain（sunshowers，Oxide，美國）等 | 活躍 | 不是快照工具，是「每個資料檔一個測試」的 harness，nextest 作者維護 | — | 原生支援 |

**選 insta 的理由：** 唯一同時有「待審檔＋互動審查＋CI 不寫入＋孤兒偵測＋結構化 redaction＋glob 語料」的工具。snapbox 的強項是 CLI 輸出與檔案樹比對，本專案主要是 JSON 和 HTML，用不到。expect-test 已封存。goldenfile 沒有審查流程，等於自己的 `OXLEDGER_UPDATE_API_SPEC` 機制。

**insta 的重點行為（官方文件）：**
- `INSTA_UPDATE` 預設 `auto`：「CI 環境為 `no`，否則為 `new`」，也就是本機跑失敗會留下 `.snap.new`，CI 完全不寫入。
- 1.48.0 起 `CI=true` 時 `cargo insta test` 視同 `--check`；明確給 `--accept` 會蓋過。
- `--unreferenced=reject` 會讓沒被任何測試用到的 `.snap` 失敗；文件說這只在跑完整測試集時有意義。
- 快照檔一律存 LF，比較前也正規化成 LF。1.46.2／1.46.3 修過 inline snapshot 遇到 `\r` 的問題。
- `cargo insta test` 預設強制所有快照測試通過以便一次收集全部改動；`INSTA_FORCE_PASS=1` 同效果。
- 建議在 `[profile.dev.package]` 把 `insta`、`similar` 設 `opt-level = 3`，diff 比較快、記憶體較少。
- 1.46.0 起可用 `INSTA_PENDING_DIR` 把待審檔寫到原始碼樹外。

來源：
- https://insta.rs/docs/advanced/
- https://insta.rs/docs/redactions/
- https://insta.rs/docs/cli/
- https://insta.rs/docs/snapshot-files/
- https://insta.rs/docs/quickstart/
- https://docs.rs/insta/latest/insta/struct.Settings.html
- https://github.com/mitsuhiko/insta/releases
- https://github.com/rust-analyzer/expect-test
- https://docs.rs/snapbox/latest/snapbox/
- https://doc.crates.io/contrib/tests/writing.html
- https://docs.rs/goldenfile/latest/goldenfile/
- crates.io API：`https://crates.io/api/v1/crates/<name>` 與 `/owners`；GitHub API：`gh api users/<login>`

### 3.2 前端

**vitest（4.1.12）**
- `toMatchSnapshot` 存到 `__snapshots__/`；`toMatchInlineSnapshot` 寫回測試原始碼；`toMatchFileSnapshot('./x.output.html')` 存成你指定副檔名的獨立檔（有語法高亮、不必跳脫）。文件提醒不要把它指到 `__snapshots__/` 這種 vitest 自己管理的路徑。
- `process.env.CI` 有設時不寫入快照；新增、不符、過時（obsolete）的快照都會讓這次執行失敗。
- 瀏覽器模式有 `toMatchScreenshot`；ARIA snapshot 從 4.1.4 起以實驗功能提供。本專案的 vitest 跑在 jsdom，不是瀏覽器模式，這兩個都用不到。
- 維護者所在地沿用 08 §5 的查證：VoidZero 已於 2026-06 被 Cloudflare 收購。

**Playwright（1.65.0）**
- `toMatchAriaSnapshot`：1.49 加入；1.50 支援存成獨立 YAML 檔、`pathTemplate`、`--update-snapshots=changed`、`--update-source-method`；1.52 加 `/children` 嚴格比對和 `/url`；1.56 比對 `placeholder`；1.62 新增 `missing` 模式；1.65 的破壞性變更：瀏覽器沒有算繪的文字不再出現在 ARIA snapshot 裡。
- 格式是 `role "name" [屬性]` 的 YAML，可以用 `/regex/`，省略的名稱或子節點代表「任何都可」。`children` 預設 `contain`（依序出現、允許多出來的），可改 `equal`／`deep-equal`。
- ARIA snapshot 與字型、反鋸齒無關，**在 Windows 上和在 Linux 上的結果應該一樣**，是本專案可以安全導入的部分。
- `toHaveScreenshot`：檔名自動帶瀏覽器和平台（例如 `-chromium-win32.png`）。官方文件：「瀏覽器算繪會依主機 OS、版本、設定、硬體而不同」，要在產生基準圖的同一環境跑。第三方文章一致建議在官方 Playwright Docker 映像產生基準圖；有 issue 回報 Windows 的字較粗、捲軸較寬。

來源：
- https://vitest.dev/guide/snapshot
- https://playwright.dev/docs/aria-snapshots
- https://playwright.dev/docs/test-snapshots
- https://playwright.dev/docs/release-notes
- https://github.com/microsoft/playwright/issues/23559
- https://argos-ci.com/blog/fix-flaky-visual-tests

### 3.3 適合與不適合 golden 的對象

| 對象 | 適合度 | 原因與做法 |
|---|---|---|
| Markdown→HTML | **最適合** | 輸出是確定性的長字串，手寫斷言只能抽查。用 `insta::glob!` 跑 `tests/golden/markdown/*.md`，每個輸入一個 `.snap`；XSS 語料也放這裡。不變式（不含 `<script>`、`on*=`）另用 proptest 或明確斷言，不要只靠快照 |
| 錯誤訊息與錯誤碼 | **很適合** | `ErrorCode` 有約 78 個值。做一份「錯誤碼 → HTTP status → 前端 i18n key」的總表快照，任何新增、改名都會在 PR diff 出現。單一錯誤路徑仍用 `assert_eq!(code(&e), ..)`，比較好讀 |
| OpenAPI spec | 已有 | 保留現有機制（見 G6） |
| migration 後的 schema | **適合** | 32 個 migration。兩種做法：(a) 用 SQL 查 `pg_catalog`／`information_schema` 輸出排序過的表、欄、約束、索引；(b) `pg_dump --schema-only`。(b) 有陷阱：PostgreSQL 16.10／17.6 起為了 CVE-2025-8714，plain 格式會插入每次隨機的 `\restrict <key>`，要加 `--restrict-key` 固定；版本標頭也會變。建議 (a) |
| HTTP API 回應 | 有條件 | 只對「形狀」有意義的端點（列表、詳情）拍，且先正規化。不要每個測試都拍整包回應：會膨脹，而且一個欄位改名會讓幾百個快照一起變，審查者只會按全部接受 |
| 搜尋結果排序 | 有條件 | 固定語料、固定查詢，快照「排序後的 issue 標籤＋四捨五入的分數」。tantivy 升級可能讓分數小幅變動，所以分數用 `rounded_redaction(2)` 或只記名次。frontier、generation、xid 等時序值不要進快照 |
| AI 執行紀錄 | 有條件 | 送給 provider 的請求（system prompt、tools 清單、body 欄位）很適合：prompt 改了要在 PR 被看到。API key 必須先遮掉並另外斷言「key 不在 body」。回應來自 `ai_fake`，是固定的，可以拍事件序列。真實模型的輸出不可拍（隨機） |
| 執行時間、效能、背景 worker 時序 | **不適合** | 本質不確定 |
| 視覺截圖 | 現階段不適合 | 只有 Windows 開發機，見 3.2 |
| 權限矩陣 | 適合（加分項） | `role × permission → allow/deny` 表格化快照，比 N 個 `assert!` 好審查 |

來源：
- https://nvd.nist.gov/vuln/detail/CVE-2025-8714
- https://gitlab.com/gitlab-org/gitlab/-/issues/563042
- https://docs.rs/crate/pg-client/0.4.0/source/src/pg_dump.rs
- https://kreya.app/blog/api-snapshot-testing/

### 3.4 業界做法與 golden 的搭配

| 做法 | 現況（2025–2026） | 和 golden 的關係 | 對本專案 |
|---|---|---|---|
| Property-based（proptest） | proptest 1.12.0（2026-10-10），組織 proptest-rs；目前活躍維護者含 Matthew Russo（AWS，美國西雅圖）、rex-remind101（美國）；原作者 Jason Lingle（AltSysrq，所在地未公開） | golden 鎖住具體例子，property 驗證所有輸入都成立的不變式。property 找到的反例可以存成 golden 語料 | 建議用在：filter AST 的 round-trip（`toGroup`／`fromGroup` 的 Rust 對應）、sanitizer 不變式、visibility 判斷。需要你同意新增依賴 |
| Mutation testing（cargo-mutants） | 27.1.0（2026-06-02），Martin Pool（Stripe）；`--in-diff` 只測 diff 範圍；`--test-tool=nextest` | 衡量「斷言有沒有在看」。快照測試通常殺 mutant 很有效，因為整個輸出都被比對；反過來，大量 redaction 會讓 mutant 存活，可用來找出 redaction 過頭 | 已核准安裝。nextest 要另開 `mutants` profile 把 `fail-fast` 打開（目前 default profile 是 `false`，官方說 fail-fast 對 mutants「非常重要」）；跑時設 `INSTA_UPDATE=no`。nextest 不跑 doctest，只被 doctest 抓到的 mutant 會顯示為 missed |
| Coverage 輔助的選擇性測試 | Google TAP、Meta predictive selection 屬大公司方案；Rust 生態的 cargo-difftests 要 nightly（05 已排除） | 快照檔本身就是「哪個測試看哪個輸出」的對照，`.snap` 改了就跑它的測試 | 用 cargo-llvm-cov（D5 候選）定期產生「原始碼檔 → 測試」的覆蓋對照，來校正 T7 的手寫對照表；不要做即時的 coverage 選擇 |
| Contract testing | Pact（pact-foundation，原作者 Ronald Holshausen，澳洲）；Schemathesis 4.20（Dmitry Dygalo，捷克布拉格，Python） | 前後端在同一 repo，`openapi.json`＋生成的 `types.ts` 就是契約，而且 svelte-check 會在型別不符時失敗 | 不需要 Pact。Schemathesis 可列為日後選項（要裝 Python 工具，需要同意） |
| Deterministic simulation testing | turmoil 0.7.2（tokio-rs，Carl Lerche，AWS，美國）；s2.dev 2025-04、Polar Signals 2025-07 的實務文章；**madsim 維護者在北京，排除** | DST 讓時序可重現，重現後的事件序列可以拍成 golden | 本專案的不確定性主要來自 PostgreSQL cluster 的 snapshot horizon，DST 模擬不到 PG。改用「可注入時鐘＋每個等待有逾時＋失敗時印出 xid 持有者」 |
| 快照審查流程 | 共識：不審就 accept 是首要失敗模式；作者要在 PR 說明每個快照為何改變；保持快照小 | — | 見 §4.4 |
| Flaky 處理 | nextest 0.9.131（2026-03-18）新增 `flaky-result = "fail"`；0.9.100 有 `global-timeout`；0.9.115 有 `on-timeout`；Windows 上逾時直接 kill，不等 grace period | redaction 不足是快照 flaky 的主要來源 | 維持 D7（`retries = 0`）；要開重試時合流 profile 一律 `flaky-result = "fail"` |

**逾時規範（9 小時事件）：** 測試在等一個永遠不會到的 gate，`cargo test` 本身沒有逾時。p2-backend 工作樹的 nextest 設定已有 `terminate-after = 10`（60 秒 × 10 = 10 分鐘）。還缺三層：
1. 測試內：每個等待（gate、channel、`sync_all`、輪詢）都用 `tokio::time::timeout`，逾時訊息寫出在等什麼。這是唯一在 `cargo test` 下也有效的一層。
2. nextest：`global-timeout`（例如 `"45m"`）防整套卡住；`m1_8_notifications` 的 120 秒迴圈用 override 給較長的 `slow-timeout`，其他維持 10 分鐘。
3. 規則：後端測試一律用 nextest 跑（`cargo test` 只用於單一測試除錯，並要自己盯著）。

來源：
- https://mutants.rs/in-diff.html
- https://mutants.rs/pr-diff.html
- https://mutants.rs/nextest.html
- https://nexte.st/docs/features/slow-tests/
- https://nexte.st/docs/features/retries/
- https://nexte.st/CHANGELOG/
- https://s2.dev/blog/dst
- https://blog.polarsignals.com/blog/posts/2025/07/08/dst-rust
- https://tokio.rs/blog/2023-01-03-announcing-turmoil
- https://pypi.org/project/schemathesis/
- https://helpmetest.com/blog/snapshot-testing-pitfalls-best-practices/

### 3.5 已知陷阱

| 陷阱 | 症狀 | 對策 |
|---|---|---|
| 快照膨脹 | 一個欄位改名，幾百個 `.snap` 一起變；PR diff 幾千行 | 只拍「這個測試要守的那一塊」，用 `json!({..})` 挑欄位再拍；單檔超過約 200 行要拆；共用欄位（例如 `meta`）在一處拍一次 |
| 無腦 accept | `cargo insta accept`／`INSTA_UPDATE=always`／`--accept` 一次接受全部 | 本機只用 `cargo insta review`；合流 `CI=true`；PR 說明逐一解釋；`always` 不寫進任何腳本 |
| redaction 不足造成 flaky | 平行跑或換時區就失敗 | 正規化函式集中在 `tests/common`；uuid v7、chrono 時間、xid、`HashMap` 迭代順序都要處理；新快照第一次產生後連跑兩次（不同 slot）確認穩定 |
| redaction 過頭 | 快照永遠通過，bug 溜過 | 不抹成同一字串，用依出現順序的標籤；cargo-mutants 存活的 mutant 會指出問題 |
| Windows 換行 | 快照在 Windows 上被 checkout 成 CRLF；或輸入語料的 `\r\n` 被 git 改掉 | insta 讀寫都正規化成 LF；`.gitattributes` 再明列 `*.snap`、`*.aria.yml` 為 `eol=lf`；**刻意測 CR 的輸入**標成 `-text` 或在測試內用 `\r\n` 字面值產生，不要放在一般文字檔 |
| D6 合併 binary 改變快照路徑 | 移到 `tests/it/` 後快照目錄和檔名前綴都變 | insta 的快照目錄相對於斷言所在檔案，檔名預設帶 module path。先做 D6 再大量遷移；或遷移時用明確名稱，合併時一次搬檔 |
| `pg_dump` 隨機 `\restrict` 鍵 | schema 快照每次都不同 | 用 catalog 查詢；或 `--restrict-key` 固定值 |
| 不確定排序 | SQL 沒有 `ORDER BY`、`HashSet` | 正規化時排序，或在 SQL 加 `ORDER BY`；insta `sort_maps(true)` |
| 快照內含密鑰 | API key、session token 被寫進 repo | 正規化時先遮；另有一個明確斷言確認原值不在輸出中 |

---

## 4. 針對 oxledger 的建議

### 4.1 工具

| 用途 | 工具 | 維護者／所在地 | 安裝 |
|---|---|---|---|
| 後端快照 | `insta`（dev-dependency，features：`json`、`redactions`、`filters`、`glob`），照專案慣例釘 `=1.49.0` | Armin Ronacher（奧地利）、Maximilian Roos（美國） | **需要你同意**（新增 crate） |
| 快照審查 CLI | `cargo-insta` 1.49.0 | 同上 | **需要你同意**（`cargo install`） |
| 不變式 | `proptest` 1.12.0 | proptest-rs（美國維護者為主） | 需要你同意；可延後 |
| mutation | `cargo-mutants` 27.1.0 | Martin Pool（Stripe） | 已核准 |
| 前端快照 | vitest 內建、Playwright 內建 | 已在使用 | 不用裝 |

### 4.2 目錄慣例

```
crates/oxledger/tests/
  snapshots/                       # insta 預設：<binary>__<name>.snap（D6 後在 tests/it/snapshots/）
  golden/                          # 輸入語料（不是快照）
    markdown/*.md                  # glob! 的輸入，每檔一個快照
    markdown/xss/*.md
    search/corpus.json, queries.txt
  common/snap.rs                   # 正規化函式（見 4.3）
web/src/lib/__snapshots__/         # vitest 預設
web/src/lib/__golden__/*.html      # toMatchFileSnapshot 的目標
web/e2e/flows.spec.ts-snapshots/   # Playwright 預設；.aria.yml
```

`.gitignore` 加：`*.snap.new`、`*.pending-snap`。
`.gitattributes` 加（明列，防 `text=auto` 判斷錯）：`*.snap text eol=lf`、`*.aria.yml text eol=lf`、`*.png binary`。

### 4.3 redaction 規則

在 `tests/common/snap.rs` 寫一個 `Normalizer`，對 `serde_json::Value` 遞迴處理，處理完再呼叫 `insta::assert_json_snapshot!`：

| 值 | 規則 | 例 |
|---|---|---|
| World 裡已知的實體 ID | 換成語意標籤 | `<user:alice>`、`<project:demo>` |
| 其他 UUID（v7） | 依第一次出現的順序編號，同一個值永遠同一個標籤 | `<uuid#1>`、`<uuid#2>` |
| 時間（RFC 3339、`created_at`、`updated_at`） | `<ts>`；如果測試在意先後，改成 `<ts#1>` 依時間排序編號 | |
| xid、frontier、generation、sequence | `<xid>`，或直接不放進快照 | |
| 浮點數（搜尋分數、成本） | 四捨五入到 2–4 位，或用 `rounded_redaction` | |
| 集合語意的陣列（tags、權限） | 排序 | |
| 密鑰、token、cookie | `<secret>`，並另有斷言確認原值不在輸出 | |
| 本機路徑、port | regex filter：`[ROOT]`、`<port>` | |

原則：**正規化必須保留參照關係**；只有真的與測試無關的欄位才整個刪掉；不用 `[..]` 萬用字元偷懶。

### 4.4 更新與審查流程

| 時機 | 指令 | 行為 |
|---|---|---|
| 開發中 | `cargo nextest run -E '<只跑自己改的部分>'` | 不符時失敗並留下 `.snap.new` |
| 審查 | `cargo insta review` | 逐一看 diff，按 a／r |
| 一次收集多個 | `cargo insta test --test-runner nextest --review -- <filter>` | 先強制通過收集，再進審查 |
| 合流全套 | `CI=true cargo insta test --test-runner nextest --check --unreferenced=reject` | 不寫入；有不符、有孤兒快照就失敗 |
| 前端合流 | `CI=true npm test`、`CI=true npx playwright test --update-snapshots=none` | 不寫入；新增或過時快照失敗 |
| 合流前檢查 | `git status --porcelain` 不得出現 `*.snap.new`、`*.pending-snap` | |

PR 規則：
- PR 說明要有「快照變更」一節，逐一寫出每個 `.snap` 為什麼改。說不出原因就是 bug 或 redaction 不足。
- 新增快照和修改快照分開 commit，審查者先看修改。
- 禁止在腳本裡寫 `INSTA_UPDATE=always`、`cargo insta accept`、`--accept`、`vitest -u`、`--update-snapshots=all`。
- 現有的 `OXLEDGER_UPDATE_API_SPEC`：`CI` 有設時忽略，避免合流時被意外覆寫。

### 4.5 和 nextest、「只測自己改的部分」的整合

- **選測試：** T7 的對照表多加一條：「改了 `tests/snapshots/X__*.snap` 或 `tests/golden/markdown/*` → 跑對應的測試」。insta 的 `--glob-filter`（或 `INSTA_GLOB_FILTER`）可以只跑某些語料。
- **孤兒偵測只在全套：** `--unreferenced=reject` 只在合流全套時開；只跑部分測試時開會把沒跑到的快照誤判成孤兒。
- **平行：** insta 每個快照是獨立檔案，nextest 每個測試一個行程，不會互相寫壞；slot DB 讓資料不互相干擾，但 ID 每次不同，所以正規化是必要的。
- **mutants：** `.config/nextest.toml` 加 `[profile.mutants]`，`fail-fast = true`；`.cargo/mutants.toml` 設 `test_tool = "nextest"`、`additional_cargo_test_args = ["--profile", "mutants"]`（實際鍵名要照 mutants.rs 文件再確認）；先只在 `--in-diff` 範圍跑，不擋合流（D12 已定 mutation 在 diff 範圍）。

### 4.6 先導遷移

| 順序 | 檔案 | 做什麼 | 預期效果 |
|---|---|---|---|
| 1 | `m2_1_markdown.rs`（p2-backend 工作樹） | XSS 語料和 pathological 輸入搬到 `tests/golden/markdown/`，用 `glob!` 產生快照；保留「不含危險標記」的明確斷言 | 刪掉自寫的 HTML tag parser 中只為比對而存在的部分；新增語料只要加一個 `.md` |
| 2 | 新增 `error_catalog` 測試（放 `s2_api.rs`） | 一份錯誤碼 → status → i18n key 的總表快照 | 錯誤碼改名、刪除在 PR 中一目了然 |
| 3 | 新增 `schema_snapshot` 測試 | migration 後查 catalog，輸出排序過的 schema | 每個 migration 的實際效果都有 diff 可審 |
| 4 | `m1_10_ai_run.rs` | 送給 provider 的請求 body（遮掉 key）拍快照；保留 key 不在 body、header 正確的明確斷言 | prompt、tool 定義的變動會被看見 |
| 5 | `p2_search.rs` 或 `m1_7_search.rs` | 固定語料的查詢結果排序 | tantivy 升級或分析器改動的影響可見；時序相關的斷言（frontier、generation）維持 `assert!` |
| 前端 1 | `filter.test.ts` | `chips`、`condText` 等文字輸出改成 inline snapshot | |
| 前端 2 | `flows.spec.ts` | 3–5 個穩定頁面（issue 詳情、佇列、AI 團隊列表）加 `toMatchAriaSnapshot`，存 `.aria.yml` | 抓到結構與無障礙名稱的改變 |

`s2_api.rs` 的 OpenAPI 檢查不遷移（見 G6）。

---

## 5. 決策表

| 編號 | 問題 | 建議 | 替代方案 | 代價／注意 |
|---|---|---|---|---|
| **G1** | 後端 golden 工具 | **insta＋cargo-insta 1.49.0** | (b) snapbox；(c) goldenfile；(d) 延用自寫的 env var 機制 | 要同意新增 crate 和安裝 cargo-insta；expect-test 已封存，不列入 |
| **G2** | insta 的 feature 與設定 | `json`、`redactions`、`filters`、`glob`；釘 `=1.49.0`；`[profile.dev.package]` 給 insta、similar `opt-level = 3` | 只開 `json`，正規化全自己做 | `glob` 會帶入 globset |
| **G3** | 目錄與命名 | insta 預設 `tests/snapshots/`；輸入語料放 `tests/golden/<主題>/`；**先做 D6 再大量遷移** | 用 `set_snapshot_path` 集中到一處 | D6 會改變快照路徑與檔名前綴 |
| **G4** | 不確定值怎麼處理 | `tests/common/snap.rs` 正規化：ID 換成依出現順序的標籤、時間 `<ts>`、xid 拿掉、浮點四捨五入、集合排序、密鑰遮罩；insta filter 只處理路徑與 port | (b) 全用 insta selector 換成固定字串 `[uuid]` | (b) 會遺失參照關係，讓「指錯 ID」的 bug 通過 |
| **G5** | 更新與審查流程 | 本機 `cargo insta review`；合流 `CI=true … --check --unreferenced=reject`；`.snap.new` 進 `.gitignore`；PR 逐一說明 | (b) 審查交給 PR diff，不用 cargo-insta | (b) 沒有互動審查，容易整批接受 |
| **G6** | 既有 OpenAPI golden | **保留**，只加「`CI` 有設時忽略 `OXLEDGER_UPDATE_API_SPEC`」 | (b) 改成 insta 快照 | (b) 會讓前端要讀帶 YAML 標頭的檔案，不合理 |
| **G7** | 先導遷移範圍 | 依 §4.6 順序：markdown → 錯誤碼總表 → schema → AI 請求 → 搜尋排序 | (b) 只做 markdown，觀察兩週再擴大 | 每一步都要先寫正規化規則 |
| **G8** | schema 快照的產生方式 | 查 `pg_catalog`／`information_schema`，在 Rust 內排序輸出 | (b) `pg_dump --schema-only --restrict-key=<固定值>` | (b) 依賴容器內 pg_dump 版本，標頭也要過濾 |
| **G9** | vitest 快照範圍 | 純函式文字輸出用 `toMatchInlineSnapshot`；長 HTML 用 `toMatchFileSnapshot`；不拍整個元件 DOM | (b) 不導入，維持 `toBe` | 元件 DOM 快照會膨脹，且和 E2E 重複 |
| **G10** | Playwright 快照 | **導入 `toMatchAriaSnapshot`（`.aria.yml`）**；`toHaveScreenshot` 暫緩 | (b) 同時導入截圖，用官方 Docker 映像產生基準圖 | (b) 要下載 Playwright 映像（需要同意），且 E2E 要能在容器內連到後端 |
| **G11** | 逾時規範 | 測試內每個等待都 `tokio::time::timeout`＋說明在等什麼；nextest 加 `global-timeout = "45m"`；後端測試一律用 nextest | (b) 只靠 nextest `terminate-after` | (b) 在 `cargo test` 下仍會無限卡住 |
| **G12** | mutation testing 怎麼接 | cargo-mutants `--in-diff`、`--test-tool=nextest`、`mutants` profile 開 fail-fast、`INSTA_UPDATE=no`；先不擋合流 | (b) 每週全量跑一次 | 慢；只測 diff 會漏掉舊的弱測試 |
| **G13** | property-based testing | 導入 proptest，先用在 filter AST round-trip、Markdown sanitizer 不變式、visibility 判斷 | (b) 暫不導入 | 要同意新增 crate；要固定 seed 與失敗案例存檔（`proptest-regressions/` 要 commit） |
| **G14** | contract testing | 不導入 Pact；以 OpenAPI golden＋`types.ts`＋svelte-check 當契約 | (b) 日後評估 Schemathesis（Python，捷克維護者）對 OpenAPI 做 fuzz | (b) 要裝 Python 工具 |
| **G15** | deterministic simulation testing | 不導入；改用可注入時鐘＋逾時＋失敗時印 xid 持有者 | (b) turmoil（tokio-rs，美國）用在 GitHub、AI provider 的網路故障模擬 | madsim 維護者在北京，排除；DST 模擬不到 PostgreSQL |
| **G16** | flaky 政策與快照 | 維持 D7 `retries = 0`；日後開重試時合流 profile 設 `flaky-result = "fail"`；新快照產生後在不同 slot 連跑兩次確認穩定 | (b) 允許 flaky 通過 | (b) 會藏住 redaction 不足 |
| **G17** | 換行 | `.gitattributes` 明列 `*.snap`、`*.aria.yml` 為 `eol=lf`、`*.png binary`；測 CR 的輸入不放一般文字檔 | (b) 只靠 `* text=auto eol=lf` 和 insta 的正規化 | (b) 在 insta 以外（`toMatchFileSnapshot`、`.aria.yml`）沒有保護 |
| **G18** | coverage 輔助選測試 | 用 cargo-llvm-cov 定期產生覆蓋對照，校正 T7 手寫表 | (b) 只維護手寫表 | 要裝 cargo-llvm-cov（D5 候選，Taiki Endo，日本） |

---

## 6. 研究過程紀錄

### 6.1 讀過的檔案（唯讀）
- `docs/research/05-testing-speed.md`、`08-review-05-06-07.md`（D1–D14、維護者查證表）
- `crates/oxledger/tests/s2_api.rs`（OpenAPI golden）、`p2_search.rs`、`m1_10_ai_run.rs`、`m2_0_prereq.rs`
- `.claude/worktrees/p2-backend-01df05/crates/oxledger/tests/m2_1_markdown.rs`、同工作樹的 `.config/nextest.toml`
- `web/package.json`、`web/package-lock.json`（版本）、`web/playwright.config.ts`、`web/src/lib/filter.test.ts`、`web/e2e/flows.spec.ts`（抽樣）
- `.gitattributes`、`.gitignore`、`crates/oxledger/Cargo.toml`、`docs/playbook.md`（CRLF 與 checksum 條目）

### 6.2 執行過的唯讀指令
- `ls`、`grep`、`wc`、`find`、`git log`、`git branch`
- `curl https://crates.io/api/v1/crates/<name>` 與 `/owners`（insta、cargo-insta、expect-test、snapbox、trycmd、goldenfile、proptest、cargo-mutants、datatest-stable、turmoil、madsim）
- `gh api users/<login>`（mitsuhiko、max-sixty、epage、calder、sourcefrog、matklad、AltSysrq、matthew-russo、rex-remind101、carllerche、wangrunji0408、sunshowers、Stranger6667、rholshausen）
- `gh api repos/<owner>/<repo>`（封存狀態、最後 push 時間）

### 6.3 搜尋與讀取
| # | 查詢或網址 | 得到什麼 |
|---|---|---|
| 1 | insta.rs/docs/advanced | `INSTA_UPDATE` 各模式、`--unreferenced`、glob |
| 2 | insta.rs/docs/redactions | selector 語法、dynamic／sorted／rounded redaction |
| 3 | github.com/mitsuhiko/insta/releases | 1.49.0（2026-10-03）、1.48 的 `CI=true` 視同 `--check`、`--nextest-profile` |
| 4 | insta.rs/docs/cli、snapshot-files、quickstart；docs.rs Settings | review／accept 鍵、LF 正規化、opt-level 建議、`bind_async` |
| 5 | WebSearch「insta snapshot testing Armin Ronacher max-sixty maintainer 2026」 | crates.io owner 確認 |
| 6 | github.com/rust-analyzer/expect-test | **2026-11-10 封存** |
| 7 | docs.rs snapbox、doc.crates.io/contrib/tests/writing | `SNAPSHOTS=overwrite`、Cargo 測試慣例 |
| 8 | docs.rs goldenfile | `UPDATE_GOLDENFILES=1`、無 redaction |
| 9 | nexte.st slow-tests、retries、CHANGELOG | `terminate-after`、`global-timeout`、`flaky-result`（0.9.131） |
| 10 | vitest.dev/guide/snapshot | `toMatchFileSnapshot`、CI 不寫入 |
| 11 | playwright.dev aria-snapshots、test-snapshots、release-notes | ARIA snapshot 版本史、截圖跨 OS 警告 |
| 12 | WebSearch「cargo-mutants 2026 incremental in-diff」；mutants.rs/nextest | `--in-diff`、nextest 搭配與 fail-fast |
| 13 | WebSearch「schemathesis maintainer」 | Dmitry Dygalo，4.20 |
| 14 | WebSearch「snapshot testing pitfalls blindly accepting」 | 審查流程共識 |
| 15 | WebSearch「deterministic simulation testing Rust 2025 turmoil」 | s2.dev、Polar Signals 文章 |
| 16 | WebSearch「pg_dump \restrict random key CVE-2025-8714」 | schema dump 不確定的原因與 `--restrict-key` |
| 17 | WebSearch「Playwright toHaveScreenshot Windows font rendering」 | 跨 OS 字型差異、Docker 建議 |

### 6.4 踩到的坑
- 指定要看的 `m2_1_markdown.rs` 不在 main，在 p2-backend 工作樹；nextest 設定也只在那裡。本報告兩者都以工作樹版本為準。
- 原本以為 expect-test 是 insta 的主要替代品，查 GitHub 才發現三天前剛封存。
- 第三方部落格（helpmetest、qaskills、testquality 等）日期不明、可信度低，只用來佐證「業界共識」，不作為功能依據；功能一律以官方文件為準。
- 網頁摘要工具對長頁面只讀前段（Playwright release notes、nextest CHANGELOG 只讀到前 10 萬字），版本資訊以已讀到的部分為準。
- madsim 在 DST 文章中常被提到，維護者 GitHub 標示北京，依規則排除。

### 6.5 沒能確認的地方
- insta 在 `INSTA_UPDATE=no` 下遇到「完全沒有快照檔」的行為：依過去經驗是失敗，本次文件頁沒有明寫；`--check` 旗標在 CLI 文件頁沒有列出，只在 1.48.0 release note 出現。導入時用一個空快照試一次。
- cargo-mutants 設定檔中帶入 nextest profile 的確切鍵名。
- proptest 原作者 Jason Lingle 的所在地（GitHub 未公開）；Ed Page 目前的雇主。
- vitest `toMatchFileSnapshot` 在 jsdom 環境寫入路徑的細節；Playwright `.aria.yml` 預設目錄名稱（文件說 `<testfile>-snapshots`）。
- 沒有實際跑任何工具，以上行為都來自文件。
