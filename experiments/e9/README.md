# E9：自訂欄位存法基準測試

比較 oxledger 自訂欄位的三種存法，回答待決問題 Q9。結果與結論見
[`docs/research/02-e9-custom-fields.md`](../../docs/research/02-e9-custom-fields.md)。

| 方案 | 表 | 說明 |
|---|---|---|
| A 型別化 EAV | `issues_a` + `issue_cf` | 一個 (issue, field) 一列，值放在對應型別的欄位；每個型別一個 partial index。先量 `(field_id, v_x)`（A 基本版），再換成 covering 的 `(field_id, v_x, issue_id)`（A covering 版）重量 |
| B JSONB | `issues_b.fields` | `{"<field_id>": value}`；GIN `jsonb_path_ops`，外加 5 個熱門欄位的 expression index |
| C 混合 | `issues_c.cf_cache` + `issue_cf` | 篩選、排序、彙總走 A 的 `issue_cf`；`cf_cache` 只是讀取用的 JSONB 副本 |

## 需求

- Docker，以及跑著 PostgreSQL 18 的容器（預設名稱 `oxledger-pg`，見 `docs/playbook.md`）。
  所有 SQL 都透過 `docker exec … psql` 送進去，本機不需要裝 psql；寫入測試用容器內的 `pgbench`。
- bash（Linux／macOS，或 Windows 的 Git Bash）、awk。
- 磁碟約 8 GB（資料庫灌完約 5 GB，加上 WAL 與暫存）。

## 怎麼跑

```bash
cd experiments/e9
./run.sh            # 全部：env → load → sizes → reads base → covering → sizes → reads covering → writes → sizes → ddl
```

也可以分段跑：`./run.sh env`、`load`、`sizes [後綴]`、`reads [標籤]`、`covering`、`writes`、`ddl`。
`load` 以外的步驟都假設資料已經灌好；`covering` 會把 A 的索引換成 covering 版（要回到基本版請重新 `load`）。
`writes` 要在 `ddl` 之前跑：`ddl` 會在交易裡改寫約 100 萬筆 `issues_b` 再 rollback，留下的膨脹會污染寫入測試。

`load` 會 **DROP 並重建** 資料庫 `oxledger_e9`（腳本拒絕操作名稱不是 `oxledger_e9` 開頭的資料庫），
不會碰 `oxledger`、`oxledger_test`。全部跑完約 50 分鐘：灌資料約 5 分鐘、讀取兩輪各約 4 分鐘、寫入約 5 分鐘，
其餘是 Q7（B 方案改寫熱門欄位每次約 3 分鐘，共 6 次）。

環境變數：

| 變數 | 預設 | 用途 |
|---|---|---|
| `PG_CONTAINER` | `oxledger-pg` | PostgreSQL 容器名稱 |
| `PGUSER` | `oxledger` | 資料庫使用者 |
| `E9_DB` | `oxledger_e9` | 資料庫名稱（必須以 `oxledger_e9` 開頭） |
| `E9_RUNS` | `15` | 每個讀取查詢的計時次數（另有 1 次暖機；`queries.sql` 可用 `runs=N` 個別覆寫） |
| `E9_WRITE_SECS` | `20` | 每輪 pgbench 寫入測試秒數 |

## 檔案

| 路徑 | 內容 |
|---|---|
| `sql/00_setup.sql` | 亂數函式（`hashint8extended`，種子固定為 42）、欄位定義、issue 基本資料、(issue, field) 配對 |
| `sql/10_load_a.sql` / `20_load_b.sql` / `30_load_c.sql` | 三種方案的表與索引 |
| `sql/15_a_covering.sql` | 把 A 的 partial index 換成 covering 版 |
| `sql/40_distribution.sql` | 灌完後印資料分布 |
| `sql/50_sizes.sql` | Q8：每個表與索引的大小 |
| `queries.sql` | Q1–Q5 與 Q9 的讀取查詢（格式說明在檔頭） |
| `ddl.sql` | Q7：刪除欄位、改型別（在 `BEGIN … ROLLBACK` 裡跑，不改資料） |
| `writes/{a,b,c}.sql` | Q6：pgbench 腳本，隨機挑一筆 issue 改一個欄位 |
| `results/` | 執行結果：`env.txt`、`distribution.txt`、`reads_{base,covering}.tsv`、`writes.tsv`、`writes_stats.tsv`、`ddl.tsv`、`sizes_*.tsv`、`explain/{base,covering}/*.txt`；以 `.` 開頭的是原始輸出 |

## 量法

- 讀取：用 psql `\timing`（容器內 unix socket，含極少量的 client 往返），同一個查詢的各方案交錯執行
  （A1 B1 A2 B2 …），讓主機上的背景雜訊平均落在各方案上；第 0 輪是暖機不計時，之後計時 `E9_RUNS` 次，
  取中位數與 p95（15 次時 p95 就是最大值）。查詢結果導到 `/dev/null`。
  之後每個查詢再跑一次 `EXPLAIN (ANALYZE, BUFFERS)` 存到 `results/explain/<標籤>/`。
- 寫入：pgbench `-M prepared`，`synchronous_commit` on／off × 1／8 個 client，各跑 `E9_WRITE_SECS` 秒，
  並用 `pg_current_wal_lsn()` 前後差算每筆交易的 WAL 量（同一個 PostgreSQL 上其他資料庫的寫入也會算進去）。測試期間關掉這幾個表的
  autovacuum，好觀察 `n_dead_tup` 和表膨脹，測完 VACUUM 再量一次，最後恢復 autovacuum。
- 資料是決定性的：所有亂數都來自 `r(key, salt)`，跟執行計畫、平行 worker 無關，重跑會得到相同資料。
