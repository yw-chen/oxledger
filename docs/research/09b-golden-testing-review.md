# 09b 獨立審查：09 Golden file 測試

日期：2026-11-14　審查對象：`docs/research/09-golden-testing.md`（及同名 `.html`）
性質：唯讀審查。沒有改程式、沒有改 09 兩份檔案、沒有安裝任何東西、沒有跑任何測試。工具行為一律查官方文件、release notes 或 **1.49.0 tag 的原始碼**；只有原始碼能確定的，以原始碼為準。
本機狀態（審查當下）：`main` HEAD `6bfec08`；已裝 cargo-nextest 0.9.151、cargo-mutants 27.1.0、cargo-llvm-cov 0.9.1、cargo-deny 0.20.2，rustup 已有 `llvm-tools` 元件；**沒有** cargo-insta。

---

## 0. 一頁結論

1. **主方向正確**：後端用 insta＋cargo-insta、自寫正規化保留參照關係、OpenAPI golden 保留、前端以 inline／file snapshot 和 ARIA snapshot 為主、截圖暫緩。版本號、`CI=true`、`--check`、`--unreferenced`、nextest 整合等關鍵主張，原始碼都證實了。
2. **錯誤與過時（要改）**：
   - Playwright 的 `missing` 模式不是 1.62 新增的。1.64 改的是它的行為：只產生缺少的快照時，測試改成通過。
   - `ErrorCode` 有 **72** 個值，不是約 78（`error.rs:202` `pub const ALL: [ErrorCode; 72]`）。
   - 後端沒有「前端 i18n key」這個欄位。對應表在 `web/src/lib/errors.ts`，Rust 測試讀不到。
   - cargo-llvm-cov 0.9.1 和 `llvm-tools` 已經裝好，G18 不必再問安裝。
   - 6.5 說 `--check` 沒列在 CLI 文件。實際上 `cargo-insta/src/cli.rs` 有這個旗標（`TestCommand.check`），算是文件頁漏寫。
3. **漏掉的重要陷阱（要補）**：
   - **feature 閘住的測試會讓 `--unreferenced=reject` 誤殺快照。** `m1_10_ai_run.rs` 等 4 個檔有 `#![cfg(feature = ...)]`。合流指令沒開 `ai,github` 時，這些快照會被判為孤兒而失敗。
   - **insta 比對前會把 `\r\n` 換成 `\n`，也會去掉結尾空白**（`snapshot.rs` `normalize()`）。所以快照看不出 CRLF 和 LF、結尾空白的差異，Markdown 的 CR 測試不能交給快照。
   - **`redactions` feature 會帶入 pest。** pest-parser 組織的公開成員有 huacnlee（Longbridge）。G4 本來就主張自己寫正規化，可以不開這個 feature。
   - `cargo insta test --test-runner nextest` 預設會另外用 `cargo test --doc` 跑 doctest（cli.rs:1007–1023），和 G11「一律用 nextest」不一致。
4. **先導順序要調整**：
   - 錯誤碼表應排第一：它在 `src` 的單元測試，不用 DB、不用正規化、已在 main。
   - Markdown 在還沒合併的 p2-backend 工作樹，不能排第一。
   - 搜尋排序應該用 `m1_7_search.rs`，不是 `p2_search.rs`。`p2_search` 全是索引生命週期、generation、GC 的測試，不適合拍快照。
5. **G3「先做 D6 再大量遷移」會把 golden 卡在 D2→D6 後面。** 改成用 `with_settings!({snapshot_path => ..., prepend_module_to_snapshot => false})` 固定路徑，就和 D6 脫鉤。
6. **沒有發現中國產品。** 唯一要標出的是 insta `redactions` 的遞移依賴 pest（見 §2.6）。

---

## 1. 主張核實

### 1.1 insta／cargo-insta

| # | 09 的主張 | 結果 | 依據 |
|---|---|---|---|
| 1 | insta、cargo-insta 最新 1.49.0（2026-10-03） | **對** | crates.io API：`max_stable_version` 1.49.0、updated 2026-10-03；GitHub release 1.49.0 發布於 2026-10-03T01:17Z |
| 2 | 下載 1.29 億次 | **對**（128,905,170） | crates.io API |
| 3 | crates.io owner 是 mitsuhiko、max-sixty | **對** | `/api/v1/crates/insta/owners` |
| 4 | `INSTA_UPDATE` 預設 `auto`：CI 為 `no`，否則 `new` | **對** | `insta/src/env.rs` `snapshot_update_behavior`：`Auto => if is_ci() { NoUpdate } else { NewFile }`。補充：還有 `force` 模式；`is_ci()` 是 `CI` 有設、而且不是 `false`／`0`／空字串，或者有 `TF_BUILD`（`utils.rs:10`） |
| 5 | 1.48 起 `CI=true` 時 `cargo insta test` 視同 `--check`，`--accept` 會蓋過 | **對** | 1.48.0 release note（#924）；cli.rs:913–920：`--accept`、`--review`、`--accept-unseen`、`--force-update-snapshots` 任一個有給就不切到 check |
| 6 | **完全沒有快照檔時，`INSTA_UPDATE=no` 會失敗**（09 §6.5 列為未確認） | **對：會失敗** | `runtime.rs`：`old_snapshot` 是 `None` 時 `pass = false`，走 `update_snapshot` 得到 `NoUpdate`，`finalize` 在沒有 `force_pass` 時 `panic!`。`cargo insta test --check` 不設 `INSTA_FORCE_PASS`（cli.rs:1354），所以合流時新增卻沒 commit 的快照會讓測試失敗 |
| 7 | `--unreferenced=reject` 擋孤兒快照 | **對，但要補條件** | cli.rs `handle_unreferenced_snapshots`。(a) 只在測試**全部成功後**才檢查。(b) 預設值是 `ignore`（`env.rs:305`），`auto` 是「CI 時 reject，否則 **delete**」，不要設成 `auto`。(c) 掃描範圍是被選中 package 的 manifest 目錄。**沒編譯進來的測試（feature 閘住、`cfg` 掉的）的快照會被當成孤兒** |
| 8 | 支援 `--test-runner nextest`；1.48 加 `--nextest-profile`；1.49 修好 `--all-targets` 搭 nextest | **對** | release notes 1.48.0（#910）、1.49.0（#460）；cli.rs:185–187 |
| 9 | redactions、filters、glob 是 feature flag | **對** | `insta/Cargo.toml`（1.49.0）：`redactions = ["pest","pest_derive","serde"]`、`filters = ["regex","strip-ansi-escapes"]`、`glob = ["walkdir","globset"]`、`json = ["serde"]`；預設只開 `colors` |
| 10 | 快照存 LF，比較前也正規化成 LF | **對，但這也是陷阱** | `snapshot.rs` `normalize()`：先 `trim_end()`，再 `replace("\r\n","\n")`。輸出從 LF 變成 CRLF，或者結尾空白改變，快照都**照樣通過**。要嚴格比對得設 `INSTA_REQUIRE_FULL_MATCH`／`--require-full-match` |
| 11 | 1.46.2／1.46.3 修過 inline snapshot 的 `\r` 問題 | **對** | CHANGELOG 1.46.2（#866）、1.46.3（#865） |
| 12 | 1.46.0 起有 `INSTA_PENDING_DIR` | **對** | CHANGELOG 1.46.0（#852） |
| 13 | `cargo insta test` 預設強制通過，以便收集全部改動 | **對** | cli.rs:1354–1355：沒有 `--check` 時設 `INSTA_FORCE_PASS=1` |
| 14 | `--check` 不在 CLI 文件頁 | **部分錯** | 旗標確實存在（cli.rs:209–210「Instructs the test command to just assert」）。文件頁有沒有列，不影響使用 |
| 15 | `[profile.dev.package]` 給 insta、similar `opt-level = 3` | 未逐字核對，和 insta 文件一貫的建議一致 | — |
| 補 | — | **新發現** | `cargo insta test --test-runner nextest` 會另外跑一次 doctest（用 cargo test），除非加 `--disable-nextest-doctest`（`--dnd`），而且 insta 已預告未來會改成預設不跑（cli.rs:1013–1023） |
| 補 | — | **新發現** | insta 依賴 `similar = "2.1"`；oxledger 已直接依賴 `similar = "=3.2.1"`。導入後 lockfile 會同時有 2.x 和 3.2.1 |

### 1.2 expect-test

| 主張 | 結果 | 依據 |
|---|---|---|
| 1.5.1，2026-11-10 | **對** | crates.io API |
| 2026-11-10 封存 | **對** | `gh api repos/rust-analyzer/expect-test`：`archived=true`，最後 push 2026-11-10T11:41Z。最後兩個 commit 分別是「Remove CI」和「Add note about vendoring…」（2026-11-10） |
| 「併回 rust-analyzer 主 repo」 | **對，但用詞要精確** | README：「This crate has been vendored into the rust-analyzer repo」。這是開發位置搬家，crate 本身還在發版（1.5.1 就是同一天發的），不等於棄用。不選它的理由應該寫成「沒有 redaction、沒有待審檔與孤兒偵測、外部使用者不是它的主要對象」，而不只是「封存」 |

### 1.3 pg_dump 與 CVE-2025-8714

| 主張 | 結果 | 依據 |
|---|---|---|
| 16.10／17.6 起 pg_dump 加入 `\restrict` | **對** | PostgreSQL 17.6、16.10 release notes（2025-08-14）都有這一條，CVE-2025-8714。同一批也含 13.22、14.19、15.14；本專案用的 PG 18 從 18.0 就有 |
| 鍵是隨機的 | **對** | pg_dump 18 文件 `--restrict-key`：「If no restrict key is specified, pg_dump will generate a random one as needed」，並說明這個選項就是給「需要可重現輸出（例如比較 dump 檔）」用的 |
| 建議改查 catalog | **同意** | 文件也警告固定鍵「不建議一般使用」，雖然只用在測試沒有風險 |

### 1.4 vitest

| 主張 | 結果 | 依據 |
|---|---|---|
| `toMatchFileSnapshot` 存成指定副檔名的獨立檔；不要指到 `__snapshots__` | **對** | vitest.dev/guide/snapshot。**補：它回傳 Promise，matcher 和測試都必須 `await`**。文件沒有寫相對路徑以什麼為基準（以測試檔為準是慣例，導入時試一次） |
| `CI` 有設時不寫入；不符、缺少、過時都會失敗 | **對** | 同頁：「By default, Vitest does not write snapshots in CI」，並且 mismatches、missing、obsolete 都會讓這次執行失敗 |
| ARIA snapshot 4.1.4 起實驗；jsdom 用不到 | **前半對；後半無法確認** | 文件標示「experimental 4.1.4+」，範例全在瀏覽器模式（`expect.element`、「the browser's accessibility tree」），但沒有明說 jsdom 不能用。結論「本專案不用」不變 |
| 版本 4.1.12 | 對（lockfile）；`package.json` 範圍是 `^4.0.0`。vitest 目前最新是 5.1.0（npm），不在範圍內 | npm registry |

### 1.5 Playwright

| 主張 | 結果 | 依據 |
|---|---|---|
| 1.49 加入 `toMatchAriaSnapshot` | **對** | release notes 1.49 |
| 1.50：獨立 YAML 檔、`pathTemplate`、`changed`、`--update-source-method` | **對** | release notes 1.50 |
| 1.52：`/children`、`/url` | **對** | release notes 1.52 |
| 1.56：比對 `placeholder` | **對** | release notes 1.56 |
| **1.62 新增 `missing` 模式** | **錯** | `missing` 早就有。**1.64** 改的是它的行為：只產生缺少快照的測試改成通過；沒給選項時的模式改名為 `default`，行為和以前一樣（寫入缺少的快照，但測試失敗）。release notes 1.64 |
| 1.65 破壞性變更：瀏覽器沒算繪的文字不再出現在 ARIA snapshot | **對** | release notes 1.65 |
| `.aria.yml` 用法 | **對，補細節** | playwright.dev/docs/aria-snapshots：`toMatchAriaSnapshot({ name: 'main.aria.yml' })`；預設放 `<spec>-snapshots/`；`expect.toMatchAriaSnapshot.pathTemplate` 可以改；`children` 預設 `contain`，可在設定檔改成全域 `equal`；「only one snapshot is saved even if testing with multiple browsers」 |
| ARIA snapshot 在 Windows 與 Linux 結果應該一樣 | **合理推論，文件沒有明說** | 文件只說跨瀏覽器只存一份。ARIA 樹不含字型或像素，所以推論可信；但不能寫成「官方保證」 |
| 合流 `--update-snapshots=none` 時「新增或過時快照失敗」 | **一半對** | 缺少的會失敗；**Playwright 沒有過時（孤兒）快照偵測**，文件沒有提供，刪掉的 `.aria.yml` 不會被發現 |
| 版本 1.65.0 | 對（lockfile）；但 `package.json` 是 `^1.56.0`，npm 最新 1.65.1 | npm registry。ARIA 語意在小版本之間會變（1.65 就改了），導入時應該釘死版本 |

### 1.6 cargo-mutants

| 主張 | 結果 | 依據 |
|---|---|---|
| 27.1.0（2026-06-02），Martin Pool（Stripe） | **對** | crates.io API；owner `sourcefrog`；GitHub company `@stripe` |
| `--in-diff` 只測 diff 範圍 | **對，補限制** | mutants.rs/in-diff：參數是 `--in-diff DIFF_FILE`（`git diff` 格式）。只比對被測程式碼，**只改測試的 diff 不會產生任何 mutant**；它只選 mutant，**不會縮小每個 mutant 要跑的測試**。也就是說，每個 mutant 還是跑全套，除非另外過濾 |
| `--test-tool=nextest` | **對** | mutants.rs/nextest；設定鍵 `test_tool = "nextest"` |
| fail-fast 對 mutants 很重要 | **對** | 同頁：「By default, nextest will fail fast, which is highly preferable…」，並建議用 profile 打開 |
| nextest 不跑 doctest，只被 doctest 抓到的 mutant 會顯示 missed | **對** | 同頁 |
| 鍵名（09 §6.5 列為未確認） | **已確認** | mutants.rs/cargo-args：`additional_cargo_args`（所有 cargo 指令）、`additional_cargo_test_args`（只給測試）。原始碼 `src/cargo.rs`：用 nextest 時，建置階段是 `cargo nextest run --no-run`、測試階段是 `cargo nextest run`，所以 `additional_cargo_args = ["--profile", "mutants"]` 兩個階段都會拿到 nextest profile；文件的寫法是 `--cargo-arg=--profile=mutants`。**陷阱：cargo-mutants 自己的 `--profile` 是 cargo 建置 profile**，會被轉成 `--cargo-profile=`，不是 nextest profile |
| 已安裝 | **對** | `cargo install --list`：cargo-mutants v27.1.0 |

### 1.7 nextest

| 主張 | 結果 | 依據 |
|---|---|---|
| `flaky-result`（0.9.131，2026-03-18） | **對** | nextest changelog |
| `global-timeout`（0.9.100） | **對** | changelog 0.9.100（2025-07-07） |
| `on-timeout`（0.9.115） | **對** | changelog 0.9.115 |
| Windows 逾時直接 kill，不等 grace period | **對** | nexte.st/docs/features/slow-tests：「On Windows, nextest terminates the test immediately in a manner akin to SIGKILL」 |
| 本機版本 | 已裝 0.9.151；crates.io 最新 0.9.152 | — |

### 1.8 專案現況描述

| 主張 | 結果 |
|---|---|
| `m2_1_markdown.rs` 在 p2-backend 工作樹，2,009 行、217 個 assert | **對**（2,008 行，相差 1 是計數方式；217 正確）。審查當下仍不在 main |
| nextest 設定只在工作樹 | **對**：main 沒有 `.config/nextest.toml` |
| `s2_api.rs` 的 OpenAPI 機制 | **對**（s2_api.rs:42–57） |
| `ErrorCode` 約 78 個值 | **錯：72 個**（`ErrorCode::ALL: [ErrorCode; 72]`；`api/types.ts` 的 `ErrorCode` union 也是 72 個） |
| 32 個 migration | **對**（`migrations/` 32 個檔） |
| 15 個前端 `*.test.ts` | **對** |

---

## 2. 補充發現

### 2.1 feature 閘門 × `--unreferenced=reject`

`m1_10_ai_run.rs`、`m1_11_e5.rs`、`m1_11_gate_ai.rs`、`s6b_github_sync.rs` 都用 `#![cfg(feature = ...)]`。合流指令要是沒有 `--features test-support,github,ai`，這些檔不會編譯；它們的 `.snap` 沒被引用，就會被判成孤兒，整次執行失敗。另外，`m1_11_e5` 需要真實的 key（08 的 07-2），而且有 `#[ignore]`。**被 ignore 或需要外部資源的測試，不應該擁有快照檔。**

### 2.2 insta 的比較會吃掉 CRLF 和結尾空白

1.1 第 10 列已經說明。Markdown 先導裡，凡是「輸入含 `\r`」「輸出結尾換行」的行為，都要保留明確的 `assert_eq!`；或者對那幾個測試設 `INSTA_REQUIRE_FULL_MATCH=1`。09 §3.5 把「insta 讀寫都正規化成 LF」只當成保護，這也是一個盲點。

### 2.3 XSS 語料不適合搬成 `.md` 檔

`m2_1_markdown.rs:332–415` 的 `XSS` 陣列含 `\u{0001}`、`\u{202e}`（RTL override）、`&#09;` 和 `\r` 類的輸入。放進一般文字檔，會受到 git 的 `text=auto`、編輯器和 `.gitattributes` 影響，有些字元還看不見。建議：

- 語料**留在 Rust 常數**裡；
- 每個樣本用穩定的名稱拍快照，或者整份語料拍成一個「輸入 → 四種模式輸出」的表格快照；
- `assert_safe`（`m2_1_markdown.rs:225`，以允許清單檢查 tag、屬性、class、href）**是安全不變式，必須完整保留**。它用的 `tags()` parser 不能刪；09 寫「刪掉只為比對存在的部分」，實際上幾乎全部都是 `assert_safe` 在用。

一般的 Markdown 範例（表格、清單、程式碼區塊）才適合放 `tests/golden/markdown/*.md`，用 `glob!` 拍。

### 2.4 錯誤碼表的實際形狀

後端有 `ErrorCode::ALL`、`as_str()`、`http_status()`、`next_actions()`，`error.rs:651` 已有 `codes_serialize_as_their_names`。把「code → status → retryable → next_actions」拍成一張表，**放在 `src/error.rs` 的單元測試**，不需要 DB、不需要正規化、不受 D6 影響。

錯誤碼的新增和改名，其實已經由 `api/types.ts` 的 OpenAPI golden 涵蓋；這張表新增的價值在 status 和 next_actions。前端的顯示文字在 `web/src/lib/errors.ts`（327 行）。要鎖住「每個 code 顯示什麼」，應該在 vitest 對 `types.ts` 的每個 code 拍一次 inline 或 file snapshot，而不是從 Rust 拍。

### 2.5 搜尋先導的檔案選錯

`p2_search.rs` 的 16 個測試全部是 generation、frontier、outbox GC、藍綠重建（例如 `blue_green_rebuild_keeps_serving`、`outbox_gc_respects_leases`），本質上和時序有關，09 自己也把它們列為不適合。排序類斷言在 `m1_7_search.rs`（例如第 257 行 `rank == Some(1)`、第 293 行「literal match ranks first」）。先導應該只用 `m1_7_search.rs`。

### 2.6 維護者與組織（不用中國產品）

| 項目 | 結果 | 依據 |
|---|---|---|
| insta、cargo-insta、similar、console | mitsuhiko（Earendil，奧地利）、max-sixty（Los Angeles）；console 另有 djc | crates.io owners；GitHub API |
| globset、walkdir（`glob` feature） | BurntSushi（美國） | crates.io owners |
| regex（`filters` feature） | rust-lang；已在 lockfile | — |
| **pest、pest_derive（`redactions` feature 的遞移依賴）** | crates.io owner 是 dragostis（Dragoș Tiselice，羅馬尼亞 Timișoara）和 `github:pest-parser:admins`。近期實際維護、發版的是 tomtau（Tomas Tauber，所在地未公開）。pest-parser 組織的**公開成員**有 huacnlee（Jason Lee，Longbridge）和 ice1000。huacnlee 最後一次 commit 是 2023-04 | `gh api orgs/pest-parser/members`、`repos/pest-parser/pest/commits` |
| proptest | owners：Centril、AltSysrq、`proptest-rs:publish`；活躍維護者 matthew-russo（AWS，Seattle）、rex-remind101（Remind，Silicon Valley） | crates.io、GitHub API |
| cargo-mutants | sourcefrog（Stripe） | 同上 |
| cargo-llvm-cov | taiki-e（日本） | GitHub API |
| expect-test | `rust-lang-owner` | crates.io |
| vitest／Playwright | 沿用 08 §5：Cloudflare（VoidZero）、Microsoft | — |

**判斷：** pest 不是中國產品，主要維護者也不在中國；只是組織成員裡有中國背景的貢獻者，而且已經多年沒有提交。依使用者規則「有疑慮就標出」，列在這裡。G4 本來就主張自己寫正規化，**不開 `redactions` 就完全不會引入 pest**，也少一個依賴。沒有發現其他疑慮。09 已經排除 madsim，這是對的。

---

## 3. 逐項意見（G1–G18）

| 編號 | 意見 | 理由 |
|---|---|---|
| G1 | **同意** | insta 是唯一同時有待審檔、互動審查、CI 不寫入、孤兒偵測的工具。expect-test 的理由改寫，見 1.2 |
| G2 | **修改**：features 只開 `json`、`glob`（`filters` 可選，regex 已在 lockfile）；**不開 `redactions`**；釘 `=1.49.0`；opt-level 建議保留；註明會和現有 `similar =3.2.1` 並存一份 2.x | 正規化由 G4 自己做，`redactions` 的 selector 用不到；同時避開 pest（§2.6） |
| G3 | **修改**：不要等 D6。所有 insta 斷言一律用**明確名稱**，再用 `with_settings!` 的 `snapshot_path` 把快照集中到 `tests/snapshots/<主題>/`，並關掉 `prepend_module_to_snapshot` | D2 和 D6 都還沒做（main 的 `test_pool()` 仍是單一 DB、72 個測試檔）。固定路徑和名稱之後，D6 搬檔不影響快照。沒給名稱時，同一個測試的多個斷言會被自動編號（`-2`、`-3`），插入新斷言就整排位移 |
| G4 | **同意，補兩點** | (1) 正規化函式要有自己的單元測試，例如同一個 UUID 出現兩次要得到同一個標籤。(2) 和 CRLF 有關的行為不靠快照（§2.2） |
| G5 | **修改合流指令**：`CI=true cargo insta test --test-runner nextest --nextest-profile <合流 profile> --dnd --features test-support,github,ai --unreferenced=reject`；明寫「`--unreferenced` 不可設成 `auto`」；`m1_11_e5` 這類 ignored 測試不擁有快照 | §2.1；`auto` 在非 CI 時會**刪檔**；`--dnd` 讓 doctest 不會繞過 nextest 的逾時；`CI=true` 已經等同 `--check`，09 再寫 `--check` 不衝突，留著當作明示 |
| G6 | **同意** | 補一句：錯誤碼的新增和改名已經由 `types.ts` 涵蓋 |
| G7 | **修改順序**（見 §4 的 G7） | §2.3–2.5：Markdown 不在 main、`p2_search` 不適合、錯誤碼表成本最低 |
| G8 | **同意** | 1.3 已核實。catalog 查詢不依賴容器裡 pg_dump 的版本 |
| G9 | **同意，補一項**：`errors.ts` 對全部 72 個 code 的顯示文字，用 vitest 拍一份 file snapshot | §2.4；`toMatchFileSnapshot` 要 `await` |
| G10 | **同意，補三點**：(1) `@playwright/test` 改成釘確切版本（目前 `^1.56.0`）；(2) 動態文字（issue key、時間、人名）在 `.aria.yml` 用 `/regex/`；(3) Playwright 不會偵測過時的 `.aria.yml`，刪測試時要手動刪檔 | 1.5；1.65 就改過 ARIA 語意，小版本升級會讓快照集體改變 |
| G11 | **同意**；`global-timeout` 的數值等 D3 量到全套時間再定，不先寫 45m | 08 的 05-4：20–30 分鐘的基準還沒量過 |
| G12 | **修改**：設定改成 `.cargo/mutants.toml` 的 `test_tool = "nextest"`、`additional_cargo_args = ["--profile", "mutants"]`（nextest 的 profile；**不要用** cargo-mutants 的 `--profile`）。另外用 `additional_cargo_test_args` 加 nextest `-E` 過濾，只跑 T7 選出的測試檔。`-j1`，等 D2 的 slot DB 做好再開 `-j`。不需要再問安裝 | 1.6：`--in-diff` 只選 mutant，不縮小測試範圍；08 的 07-1 已算出 50 個 mutant 要 1–2.5 小時 |
| G13 | **同意**（延後、需要同意新增 crate） | 維護者沒有疑慮 |
| G14 | **同意** | — |
| G15 | **同意** | — |
| G16 | **同意** | 和 D7 一致 |
| G17 | **同意，補一句**：insta 比較會把 CRLF 正規化、去掉結尾空白，CR 相關的行為用明確斷言 | §2.2 |
| G18 | **修改**：cargo-llvm-cov 0.9.1 和 `llvm-tools` 已裝，刪掉「要裝」。執行仍要約全套測試的時間，排在 D2 之後、每月或每個里程碑跑一次 | 本機 `cargo install --list`、`rustup component list` |

**和 D1–D14 的衝突：** 沒有直接衝突。要注意的有兩點：

- G3 原寫法會讓 golden 依賴 D6、間接依賴 D2；改成固定 `snapshot_path` 後就沒有依賴。
- G12 必須遵守 D5、D12「mutants 等 D2 完成」。工具已經裝好，不代表可以提早開平行。

**過度設計：** G13–G15 屬於延後項，放在決策表裡沒有問題。§4.4「新增快照和修改快照分開 commit」對單人加 agent 的流程成本偏高，建議改成「PR 說明分兩節列出」就好。

**漏掉的選項：**

- `datatest-stable`（nextest 作者維護）可以讓每個語料檔成為一個獨立測試，nextest 的 `-E` 就能只跑改到的語料。Markdown 語料量目前不大，`glob!` 加 `--glob-filter` 已經夠用，列為日後選項即可。
- 沒有發現更好的 Rust 快照工具。

---

## 4. 建議的最終決策表

| 編號 | 題目 | 建議 | 代價／前置 |
|---|---|---|---|
| G1 | 後端 golden 工具 | insta＋cargo-insta 1.49.0 | 要同意新增 dev-dependency 和安裝 cargo-insta（尚未安裝） |
| G2 | insta features | `json`、`glob`（`filters` 可選）；**不開 `redactions`**；釘 `=1.49.0`；`[profile.dev.package]` 給 insta、similar `opt-level = 3` | lockfile 會多一份 similar 2.x |
| G3 | 目錄與命名 | 斷言一律具名；`with_settings!` 固定 `snapshot_path = tests/snapshots/<主題>`、不加 module 前綴；語料放 `tests/golden/<主題>/`；**不等 D6** | 每個測試檔多一行設定 |
| G4 | 不確定值 | `tests/common/snap.rs` 自寫正規化：依出現順序的標籤、`<ts>`、去掉 xid、浮點四捨五入、集合排序、密鑰遮罩並另外斷言原值不在輸出；正規化函式本身要有單元測試 | — |
| G5 | 更新與審查 | 本機 `cargo insta review`；合流 `CI=true cargo insta test --test-runner nextest --nextest-profile <合流> --dnd --features test-support,github,ai --unreferenced=reject`；`--unreferenced` 不用 `auto`；`.snap.new`、`.pending-snap` 進 `.gitignore`；PR 說明列出每個快照改動 | ignored 或需要外部資源的測試不擁有快照 |
| G6 | OpenAPI golden | 保留；`CI` 有設時忽略 `OXLEDGER_UPDATE_API_SPEC` | — |
| G7 | 先導順序 | (1) `src/error.rs` 錯誤碼表（code → status → retryable → next_actions），加前端 `errors.ts` 的 vitest 快照；(2) `m1_10_ai_run.rs` 的 provider 請求 body（遮掉 key，保留 header 和 key 的明確斷言）；(3) `m2_1_markdown`，**等 p2-backend 合併後**做：一般範例走 `glob!`，XSS 語料留在 Rust，`assert_safe` 與 `tags()` 完整保留，CR 行為用明確斷言；(4) `m1_7_search.rs` 的固定語料排序（不碰 `p2_search.rs`）；(5) schema catalog 快照 | 第 2 步要在合流開 `ai` feature |
| G8 | schema 快照 | 查 `pg_catalog`／`information_schema`，在 Rust 內排序 | — |
| G9 | vitest | 純函式用 `toMatchInlineSnapshot`；長 HTML 和 `errors.ts` 對照用 `await toMatchFileSnapshot`；不拍元件 DOM | — |
| G10 | Playwright | 導入 `toMatchAriaSnapshot({ name: '*.aria.yml' })`，3–5 個穩定頁面，動態文字用 regex；釘確切 Playwright 版本；合流 `--update-snapshots=none`；截圖暫緩 | 沒有孤兒偵測，刪測試要手動刪檔 |
| G11 | 逾時 | 每個等待用 `tokio::time::timeout`；nextest `global-timeout` 等 D3 量測後再定；後端一律用 nextest | — |
| G12 | mutation | `test_tool = "nextest"`、`additional_cargo_args = ["--profile","mutants"]`（nextest profile 開 fail-fast）、加 `-E` 測試過濾、`--in-diff`、`-j1`，等 D2 完成；不擋合流 | 已安裝；成本見 08 的 07-1 |
| G13 | proptest | 延後；導入時用在 filter AST round-trip、sanitizer 不變式、visibility | 要同意新增 crate；`proptest-regressions/` 要 commit |
| G14 | contract testing | 不導入 Pact；OpenAPI golden、`types.ts` 加 svelte-check 就是契約 | — |
| G15 | DST | 不導入；可注入時鐘＋逾時＋印出 xid 持有者 | — |
| G16 | flaky | 維持 D7（`retries = 0`）；日後開重試時合流 profile 設 `flaky-result = "fail"`；新快照在不同 slot 連跑兩次 | — |
| G17 | 換行 | `.gitattributes` 明列 `*.snap`、`*.aria.yml` 為 `eol=lf`；CR 行為不靠快照（insta 比較會把 CRLF 正規化、去掉結尾空白） | — |
| G18 | coverage 輔助選測試 | 用已安裝的 cargo-llvm-cov 0.9.1，在 D2 之後定期產生覆蓋對照，校正 T7 | 不需要安裝；每次約一輪全套的時間 |

---

## 5. 審查過程

- **讀過：** 09 的 `.md` 全文、`.html`（抽查數字）；08 的 §0、§1–§6；`crates/oxledger/tests/{s2_api,p2_search,m1_10_ai_run,m1_7_search}.rs`（抽讀）、`tests/common/mod.rs`；工作樹的 `m2_1_markdown.rs`（第 200–500 行和函式清單）；`src/error.rs`；`web/src/lib/errors.ts`、`web/package.json`、`vite.config.ts`、`playwright.config.ts`；`Cargo.toml`、`Cargo.lock`、`.gitignore`。
- **指令（唯讀）：** `git log`、`cargo install --list`、`rustup component list --installed`、`cargo-nextest --version`、grep、wc；crates.io API（versions、owners）；`gh api`（repos、releases、contents@tag、users、orgs members、commits）；npm registry（latest）。
- **一手來源：**
  - insta 1.49.0 原始碼：`insta/Cargo.toml`、`insta/src/{env,runtime,snapshot,utils}.rs`、`cargo-insta/src/{cli,walk}.rs`；CHANGELOG 和 GitHub releases；
  - cargo-mutants v27.1.0 `src/cargo.rs`，以及 mutants.rs 的 `nextest`、`cargo-args`、`in-diff` 三頁；
  - nextest changelog（main），以及 nexte.st 的 slow-tests 頁；
  - vitest.dev 的 `guide/snapshot`、`guide/browser/aria-snapshots`；
  - playwright.dev 的 `docs/aria-snapshots`，以及 repo 的 `docs/src/release-notes-js.md`；
  - postgresql.org 的 16.10、17.6 release notes 和 `docs/18/app-pgdump.html`；
  - `rust-analyzer/expect-test` 的 README 和 commits。
- **沒做：** 沒有實際執行 insta 或 Playwright。「jsdom 下 vitest ARIA snapshot 能不能用」「`toMatchFileSnapshot` 相對路徑的基準」兩點仍未確認，留待導入時試一次。
