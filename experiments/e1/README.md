# 實驗 E1：繁體中文檢索品質

比較 oxledger BM25 搜尋的中文斷詞方案。報告在 [`docs/research/03-e1-zh-retrieval.md`](../../docs/research/03-e1-zh-retrieval.md)。

不連資料庫：每個方案在暫存目錄（`%TEMP%/oxledger-e1-*`，跑完刪除）建一個 tantivy 索引，不會碰 `oxledger`、`oxledger_test`、`oxledger_e9`。

## 內容

| 路徑 | 說明 |
|---|---|
| `data/issues.jsonl` | 200 筆合成的繁中 issue（`id`、`subject`、`description`；`tags` 只是標註輔助，不進索引） |
| `data/queries.jsonl` | 60 個查詢，`type` 為 exact／synonym／mixed／single／decoy，`rels` 是 issue id → 相關度（2 高度相關、1 部分相關，沒列的是 0） |
| `data/aliases.jsonl` | 別名展開表（E2 雛形），查詢字串包含 `term` 時加入 `expand` |
| `src/schemes.rs` | 斷詞方案 a、a2、b、c（直接重用 `oxledger::search::tokenize`／`tokenize_query`），d（ICU4X）、e（lindera + CC-CEDICT） |
| `src/main.rs` | 建索引、查詢（與 M0 相同的查詢形狀：每個詞 Should，標題 ×3、描述 ×1）、算指標、輸出報表 |
| `results/summary.md` | 最近一次的完整結果表 |
| `results/per_query.jsonl` | 每個 run × 查詢的指標、前 10 名（附相關度）、前 20 名漏掉的相關 issue |

## 怎麼跑

Rust 在 `~/.cargo/bin`，PATH 沒設的話先加：

```bash
export PATH="$HOME/.cargo/bin:$PATH"
```

只跑 a、a2、b、c（不需要額外下載）：

```bash
cd experiments/e1 && cargo run --release
```

連 d、e 一起跑（第一次會從 crates.io 下載 `icu_segmenter`、`lindera`，`lindera-cc-cedict` 的 build script 還會從 lindera.dev 下載 CC-CEDICT-MeCab 詞典）：

```bash
cd experiments/e1 && cargo run --release --features icu,lindera
```

看各方案怎麼斷某段文字：

```bash
cd experiments/e1 && cargo run --release --features icu,lindera -- --tokens "無法登入" "工時報表"
```

結果寫到 `results/`（可用 `--out DIR` 改位置），同時印出 `summary.md`。在這台 Windows 11 機器上整個流程約 15 秒（不含第一次編譯）。延遲數字每次跑會有幾 µs 的抖動，品質指標是確定性的。

## 指標

- **nDCG＠10**：增益 2^g − 1，理想排序取全部判定的相關 issue。
- **recall＠20**：前 20 名裡相關（g ≥ 1）的比例。
- **MRR**：第一個相關結果名次的倒數。
- **P＠10**：附帶；相關 issue 少於 10 筆的查詢，P＠10 上限本來就低。
- **配對比較**：每個 run 對基線 a 的 nDCG＠10 逐查詢比勝負，加 bootstrap（2000 次、固定種子）95% 信賴區間。
- **成本**：200 筆與放大 50 倍（10,000 筆，重複語料）時的索引大小、建索引時間、查詢延遲 p50／p95（含斷詞、搜尋、讀 stored field）。

## 授權注意

- `icu_segmenter`：Unicode-3.0，可與 GPL-2.0-or-later 相容。
- `lindera`：程式碼 MIT；**CC-CEDICT 詞典是 CC BY-SA 4.0，和 GPL-2.0 不相容**。方案 e 只供實驗，不要把 `lindera` feature 帶進產品。
