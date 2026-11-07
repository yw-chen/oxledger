# SD：搜尋一致性、explain 與 D14 效能尾巴的實測

設計文件 [`docs/design/03-search-consistency-explain.md`](../../docs/design/03-search-consistency-explain.md) 的實測程式與原始結果。2026-11-06，PostgreSQL 18.6（Docker 容器 `oxledger-pg`，映像預設設定，`shared_buffers=128MB`），tantivy 0.27.0。

全部只在暫時資料庫 `oxledger_sd_bench` 上跑（`load.sh` 拒絕其他名稱）。量完已經刪掉。

| 檔案 | 用途 | 結果 |
|---|---|---|
| `frontier.sh` | 哪些開啟中的交易會擋住 outbox 的 safe frontier `pg_snapshot_xmin(pg_current_snapshot())`（同／跨 database、有無 xid） | `results/frontier.txt` |
| `cic_frontier.sh` | `CREATE INDEX CONCURRENTLY` 期間 frontier 會不會被擋 | `results/cic_frontier.txt` |
| `load.sh`、`sql/00_setup.sql`（從 `experiments/e9` 複製）、`sql/10_load.sql` | 100 萬筆 issue、`issue_cf` 1,770 萬列，用 E9 的產生器，但改成 D14 的正式形狀（每欄位一個 covering partial index、list 存 `v_ref`、`(field_id, v_ref)` MCV） | 灌入約 5.5 分鐘，資料庫約 2.4 GB |
| `sql/est.sql` | 單一條件的列數估計與實際值 | `results/est.txt` |
| `q/gen.sh`、`q/*.sql`、`bench.sh` | 多條件 + `ORDER BY id DESC LIMIT 50`：S0（現行 EXISTS）、S1（`IS TRUE` 包起來）、S2（每個 EXISTS 加 `OFFSET 0`）；計數查詢；上限 10,001 的選擇性探測 | `results/q3.tsv`（名稱、中位數、最大值、最小值，單位 ms），`results/explain/*.txt` |
| `sql/stats_try.sql`、`sql/stats_try2.sql` | 統計修正的嘗試（statistics target、partial expression index、CASE 運算式統計） | `results/stats_try*.txt` |
| `sql/retype.sql` | 改型別：分批就地轉換（數字 → 文字）、`CREATE INDEX CONCURRENTLY`，再對照單一 UPDATE | `results/retype*.txt` |
| `tantivy-probe/` | tantivy 0.27 的 `SnippetGenerator`、`Explanation`、commit payload 實際行為（離線編譯：`cargo build --release --offline`） | `results/tantivy-probe.txt` |

## 重跑

```bash
cd experiments/sd
docker exec -i oxledger-pg psql -U oxledger -d postgres -c "CREATE DATABASE oxledger_sd_bench"
./frontier.sh
./load.sh
docker exec -i oxledger-pg psql -X -U oxledger -d oxledger_sd_bench < sql/est.sql
bash q/gen.sh
for f in three hot2 hotcold numdate num2 rareint; do for s in S0 S1 S2 count; do ./bench.sh ${f}_$s 15 < q/${f}_$s.sql; done; done
./cic_frontier.sh
docker exec -i oxledger-pg psql -X -U oxledger -d oxledger_sd_bench < sql/stats_try.sql
docker exec -i oxledger-pg psql -X -U oxledger -d oxledger_sd_bench < sql/retype.sql   # 會改掉欄位 1、8 的型別，放最後
docker exec -i oxledger-pg psql -U oxledger -d postgres -c "DROP DATABASE oxledger_sd_bench"
```

注意事項：
- `sql/retype.sql` 會把欄位 1、8 轉成文字，必須最後跑。
- `rareint` 用欄位 3、9，所以不受轉型影響。
- `bench.sh` 每個查詢先跑 1 次暖機，再計時 N 次。
- `results/q3.tsv` 裡 `hotcold_S1`、`hotcold_S2` 因為太慢，只跑了 5 次和 3 次；`rareint_*` 跑 7 次。
