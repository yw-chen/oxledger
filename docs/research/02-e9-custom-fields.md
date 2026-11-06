# E9：自訂欄位存法實測（型別化 EAV vs JSONB）

2026-11-06。回答待決問題 Q9（`01-redmine-rust-rewrite.md` §5.5、§9）。實驗程式在
[`experiments/e9/`](../../experiments/e9/README.md)，`./run.sh` 一鍵重跑；原始數字在 `experiments/e9/results/`。

## 結論先講

- **Q9 建議選 A：型別化 EAV，partial index 要做成 covering `(field_id, v_x, issue_id)`。** 不加 JSONB 讀取快取（C）。
- 決定性的差距在**冷門欄位**和**寫入**，這兩件事 B 沒有補救辦法：
  - 1000 個欄位裡 811 個的覆蓋率低於 1%。對這些欄位做範圍篩選、排序、group by，A 是 1–5 ms，B 一律全表掃描，
    100 萬筆時 100–300 ms，而且會隨 issue 數線性變慢。B 要追上，就得對每個會被篩選的欄位建 expression index，等於回到「一欄一索引」。
  - 寫入（8 個並行 client，`synchronous_commit=off`）A 8,313 tps，B 2,257 tps（GIN pending list 讓並行寫入排隊）；
    同步提交時 A 1,163、B 353 tps。B 每次更新都不是 HOT，每次編輯留下的膨脹約是 A 的 4.4 倍。
  - 刪除熱門欄位（95 萬筆有值）A 約 18 秒，B 約 3.5 分鐘，而且 B 的整張 issues 表在這段期間被逐列改寫。
- B 贏的地方：**熱門欄位**而且**剛好有 expression index** 時，篩選與排序快 2–3 倍（例如 0.3 vs 0.6 ms、3 vs 14 ms）；
  在可見範圍（約 11 萬筆）上做加總、group by 快 1.2–3.6 倍；儲存小 3.1 倍（865 MB vs 2,715 MB）。
  這些差距多半在毫秒級，或是本來就規劃交給 tantivy 做的 facet／彙總（§5.5）。
- covering index 是 A 能用的前提：同一批查詢，A 基本版（任務原本指定的 `(field_id, v_x)`）在多欄位組合時 0.7–1.3 秒，
  換成 covering 後 0.2 秒以內或更低，代價是索引多 590 MB。
- 混合方案 C 只在「列表頁一次取 50 筆的全部欄位」快約 1 ms（0.66 vs 1.71 ms），卻讓 8 個 client 的寫入從 8,313 掉到 3,246 tps。不值得。

## 1. 環境

| 項目 | 值 |
|---|---|
| 主機 | Intel Core i7-8700（6C/12T）、32 GB RAM、Windows 11 Pro；桌機，背景有瀏覽器等程式（CPU 約 25–30%） |
| PostgreSQL | 18.6（Debian 18.6-1.pgdg13+2），Docker Desktop 29.5.3 的 `oxledger-pg` 容器，12 vCPU、15.6 GB RAM |
| 設定 | **映像預設值、沒調**：`shared_buffers=128MB`、`work_mem=4MB`、`max_parallel_workers_per_gather=2`、`jit=on`、`random_page_cost=4`、`synchronous_commit=on` |
| 資料庫 | `oxledger_e9`（獨立資料庫；同一個 PostgreSQL 上另有 `oxledger`、`oxledger_test`，量測期間抽查幾次都沒看到它們有查詢在跑） |

`shared_buffers` 只有 128 MB，而 A 的資料加索引約 2.8 GB、B 約 1 GB，所以大部分 `Buffers: read` 是從容器的 OS page cache
讀（不是磁碟），這點會影響數字，見 §6。沒改設定是因為容器跟其他 session 共用，改 `shared_buffers` 要重啟。

## 2. Schema

三個方案共用同一份 issue 基本欄位（形狀跟 `migrations/0001_init.sql` 的 `issues` 一致，id 是時間排序的 uuid），各自一張表，避免互相影響。
完整 DDL 在 `experiments/e9/sql/`。

**A 型別化 EAV**（`10_load_a.sql`、`15_a_covering.sql`）

```sql
CREATE TABLE issue_cf (
    issue_id uuid NOT NULL REFERENCES issues_a (id) ON DELETE CASCADE,
    field_id int  NOT NULL REFERENCES cf_defs (id) ON DELETE CASCADE,
    v_date date, v_bool boolean, v_ref uuid, v_num numeric, v_text text,  -- 定長欄位在前，少一點 padding
    PRIMARY KEY (issue_id, field_id),
    CHECK (num_nonnulls(v_num, v_date, v_text, v_ref, v_bool) = 1)
);
-- A 基本版：每個型別一個 partial index
CREATE INDEX issue_cf_num ON issue_cf (field_id, v_num) WHERE v_num IS NOT NULL;   -- date/text/ref/bool 同理
-- A covering 版（本報告的「A」）：同樣五個，尾端多帶 issue_id
CREATE INDEX issue_cf_num_cov ON issue_cf (field_id, v_num, issue_id) WHERE v_num IS NOT NULL;
```

- 數字一律 `numeric`（Redmine 的 int 與 float 都放得下，排序與加總不用轉型）。
- list 存選項文字（`v_text`）。正式版建議存選項 id，見 §5。
- 篩選寫成每個欄位一個 `EXISTS (SELECT 1 FROM issue_cf c WHERE c.issue_id = i.id AND c.field_id = $f AND c.v_x <op> $v)`。

**B JSONB**（`20_load_b.sql`）

- `issues_b.fields jsonb`：`{"<field_id>": 值}`，數字存 JSON number，日期存 `"YYYY-MM-DD"`，user 存 uuid 字串，bool 存 JSON boolean。
- `GIN (fields jsonb_path_ops)`：比 `jsonb_ops` 小，支援 `@>`、`@?`、`@@`，不支援 `?`（key 存在）。兩種 opclass 都不支援範圍與排序。
- expression index 只建在 5 個熱門欄位：`((fields->>'1')::numeric)`、`(fields->>'2')`、`(fields->>'3')`、`(fields->>'4')`、`((fields->>'8')::numeric)`。
  日期用 ISO 文字比較，因為 `text::date` 不是 IMMUTABLE（受 DateStyle 影響），不能建索引。
- 等值篩選用 `@>`（走 GIN）；範圍與排序用 `->>` 加轉型（有 expression index 才走得到索引）。表中 **Bx** 是改用 `->>` 走 expression index 的寫法，**Bp** 是 jsonpath `@@`。

**C 混合**（`30_load_c.sql`）：A 的 `issue_cf` 加上 `issues_c.cf_cache jsonb`（無索引），只在顯示時讀。篩選、排序、彙總與 A 完全相同，所以只量了 Q6 寫入、Q8 大小與 Q9 列表顯示。

## 3. 資料

固定種子 42（`hashint8extended`，與執行計畫、平行 worker 無關，重跑資料相同）。`results/distribution.txt`：

| 項目 | 值 |
|---|---|
| issue | 1,000,000 筆，200 個專案（大小偏斜），500 個使用者，10% private |
| 欄位定義 | 1000 個：num 253、list 251、text 209、date 146、user 100、bool 41 |
| 欄位值 | 17,750,622 個；每筆 issue 平均 17.8 個（p5 12、p50 18、p95 24、最多 28） |
| 熱門欄位 | 1–10 號依設定機率出現（60–95%）；另外約 8 個低編號的冷門欄位因抽樣偏斜也變得常見 |
| 覆蓋率分布 | ≥50%：11 個；10–50%：7 個；1–10%：171 個；0.3–1%：709 個；<0.3%：102 個 |
| JSONB 大小 | `fields` 平均 361 bytes、最大 731 bytes（都沒進 TOAST） |
| 可見範圍 | 專案 101–130，且 private 只看自己（`uid(7)`）寫的：108,428 筆（10.8%） |

量測用的欄位：1 num（95%）、2 date（90%）、3 list 5 選項（95%）、4 user（85%）、8 num（80%）；冷門的 800 num、806 date、807 list 41 選項，各約 3,200 筆（0.32%）。

## 4. 結果

### 4.1 讀取（Q1–Q5、Q9）

每格是「中位數 / p95」毫秒，15 次計時（另有 1 次暖機），同一查詢的各方案交錯執行。**A0** 是 A 基本版（第一輪），
**A** 是 covering 版（第二輪）；B、Bx、Bp、C 取第二輪（理由見 §6.1）。原始資料：`results/reads_{base,covering}.tsv`。
「count」是算總筆數（分頁要用），「page」是 `ORDER BY id DESC LIMIT 50`。

| 查詢 | 符合筆數 | A0 | **A** | B（GIN／seq） | Bx（expr index） | 較快者 |
|---|---:|---:|---:|---:|---:|---|
| Q1 熱門 user = X，count | 11,951 | 120.5 / 126.6 | 15.2 / 15.8 | 12.5 / 12.9 | 8.6 / 9.3 | Bx 1.8× |
| Q1 熱門 user = X，page | | 10.6 / 44.8 | **0.41** / 0.47 | 1.75 / 3.30 | 1.41 / 1.58 | A 3.4× |
| Q1 冷門 list = X，count | 450 | 1.28 / 1.59 | 0.98 / 1.36 | 0.64 / 0.93 | — | B 1.5× |
| Q1 冷門 list = X，page | | 1.32 / 1.67 | **0.37** / 0.48 | 0.70 / 0.78 | — | A 1.9× |
| Q2 熱門 num 範圍，count | 8,123 | 168.4 / 191.5 | 13.9 / 27.3 | Bp 241.2 / 255.5 | 6.4 / 33.0 | Bx 2.2× |
| Q2 熱門 num 範圍，page | | 19.2 / 20.6 | 13.9 / 16.9 | — | **2.96** / 4.03 | Bx 4.7× |
| Q2 熱門 date 範圍，count | 9,758 | 103.6 / 110.9 | 15.4 / 18.7 | — | 7.5 / 118.3 | Bx 2.1× |
| Q2 熱門 date 範圍，page | | 140.7 / 148.1 | **2.40** / 2.70 | — | 8.42 / 8.85 | A 3.5× |
| Q2 冷門 num 範圍，count | 305 | 1.20 / 1.97 | **0.98** / 1.51 | 123.6 / 127.5（Bp 176.0） | — | A 126× |
| Q2 冷門 num 範圍，page | | 1.26 / 1.38 | **0.94** / 1.17 | 37.4 / 38.0 | — | A 40× |
| Q2 冷門 date 範圍，count | 197 | 0.74 / 0.96 | **1.08** / 1.56 | 117.0 / 140.3 | — | A 108× |
| Q3 可見 + 熱門 2 欄，count | 10,142 | 721.7 / 772.1 | **200.5** / 212.7 | 298.4 / 304.8 | — | A 1.5× |
| Q3 可見 + 熱門 2 欄，page | | 1292.1 / 1347.5 | 1.80 / 3.66 | **1.35** / 1.79 | — | B 1.3× |
| Q3 可見 + 熱門 + 冷門，count | 21 | 42.1 / 44.5 | **24.7** / 30.5 | 289.9 / 309.4 | — | A 12× |
| Q3 可見 + 熱門 + 冷門，page | | 42.7 / 45.4 | **22.2** / 25.8 | 298.6 / 311.8 | — | A 13× |
| Q3 可見 + 熱門 3 欄，count | 6,478 | 674.4 / 753.6 | **216.2** / 265.5 | 311.1 / 381.7 | — | A 1.4× |
| Q3 可見 + 熱門 3 欄，page | | 730.2 / 790.3 | 216.0 / 225.8 | **2.90** / 3.79 | — | B 75× |
| Q4 熱門 num 排序（全部） | 50 | 0.63 / 0.88 | 0.62 / 0.90 | — | **0.28** / 0.44 | Bx 2.2× |
| Q4 熱門 num 排序（可見） | 50 | 2.41 / 2.72 | 2.43 / 2.88 | — | **0.84** / 0.99 | Bx 2.9× |
| Q4 冷門 num 排序（可見） | 50 | 6.75 / 7.88 | **5.08** / 6.53 | 104.3 / 119.2 | — | A 21× |
| Q4 可見 + list 篩選，依 date 排序 | 50 | 77.3 / 87.4 | 104.2 / 134.5 | — | **5.53** / 10.3 | Bx 19× |
| Q5 熱門 list group by（全部） | 5 組 | 41.7 / 48.8 | **55.3** / 77.5 | 182.6 / 185.8 | — | A 3.3× |
| Q5 熱門 list group by（可見） | 5 組 | 387.7 / 533.3 | 136.1 / 161.1 | **114.8** / 147.7 | — | B 1.2× |
| Q5 冷門 list group by（全部） | 41 組 | 0.56 / 0.60 | **0.60** / 0.83 | 297.0 / 306.8 | — | A 495× |
| Q5 熱門 num 加總（可見） | 1 | 735.9 / 769.9 | 152.9 / 165.4 | **116.8** / 128.1 | — | B 1.3× |
| Q5 group by list + 加總 num（可見） | 5 組 | 476.2 / 537.3 | 479.6 / 509.5 | **132.8** / 179.3 | — | B 3.6× |
| Q9 列表 50 筆含全部欄位 | 50 | 1.76 / 3.35 | 1.71 / 2.22 | **0.65** / 0.84 | — | B、C 2.6× |

Q9 的 C（讀 `cf_cache`）：0.66 / 0.75 ms，與 B 相同。

### 4.2 寫入（Q6）

隨機挑一筆 issue 改欄位 8（數字），同一個交易也更新 `lock_version`、`updated_at`（實際編輯 issue 一定會動這兩個）。
A：`UPDATE issues_a` + `INSERT … ON CONFLICT DO UPDATE` 到 `issue_cf`；B：一個 `UPDATE` 用 `jsonb_set`；C：A 的兩句再加 `jsonb_set` 到 `cf_cache`。
pgbench `-M prepared`，每格 20 秒。原始資料：`results/writes.tsv`、`results/.pgbench_*`。

| 方案 | sync on，1 client | sync on，8 clients | sync off，1 client | sync off，8 clients |
|---|---:|---:|---:|---:|
| A | 248 tps（4.0 ms） | 1,163 tps（6.9 ms） | 2,315 tps（0.43 ms） | **8,313** tps（0.96 ms） |
| B | 102 tps（9.8 ms） | 353 tps（22.7 ms） | 1,901 tps（0.53 ms） | 2,257 tps（3.5 ms） |
| C | 202 tps（5.0 ms） | 1,060 tps（7.5 ms） | 1,535 tps（0.65 ms） | 3,246 tps（2.5 ms） |

括號是平均延遲。`synchronous_commit=on` 時單一 client 被 commit 的 fsync 綁住（約 4 ms），看不太出存法差異；off 才看得到 CPU 與鎖的差別。
B 加到 8 個 client 幾乎沒變快。推測原因（沒有另外驗證，例如關掉 `fastupdate` 重量）：每次更新都要把新 tuple 的約 18 個 key/value 寫進 GIN，`fastupdate` 讓它們先進 pending list，
大家搶同一個 metapage，滿 4 MB（`gin_pending_list_limit`）時由剛好寫入的那個 backend 負責清，延遲因此忽高忽低。

膨脹（關掉 autovacuum，`results/writes_stats.tsv`；A 和 C 共用 `issue_cf`，C 的數字是在 A 之後再量的增量）：

| | A | B |
|---|---|---|
| 測試期間更新次數 | `issues_a` 242,012、`issue_cf` 198,552 更新 + 43,460 新增 | `issues_b` 101,047 |
| HOT 更新 | `issues_a` 220,985（91%）；`issue_cf` 1 | 0 |
| 留下的 dead tuple | `issues_a` 41,006、`issue_cf` 198,551 | 101,047 |
| heap 成長 | `issues_a` +3.3 MB、`issue_cf` +8.6 MB | +38.3 MB |
| 索引成長 | +37.8 MB（全在 `issue_cf`；`num_cov` 194 → 232 MB） | +52.5 MB |
| 每次編輯平均成長 | 約 216 bytes | 約 942 bytes |

- B 的 `fields` 有 GIN 和 expression index，任何欄位改動都不可能是 HOT，整個約 400 bytes 的 tuple 連同全部 8 個索引都要重寫。
- A 的 `issues_a` 只改沒有索引的欄位，九成是 HOT；`issue_cf` 那一列的值在 covering index 裡，所以也不是 HOT，但列只有約 60 bytes。
- VACUUM 後 dead tuple 歸零，但檔案不會縮，空間留給之後重用。
- 每筆交易的 WAL 量（14–37 KB）主要是 checkpoint 後第一次改到某頁時的 full-page image，隨機更新幾乎每次都碰到新頁，所以這個數字看不出兩個方案的差異，沒列進比較。

### 4.3 刪除欄位與改型別（Q7）

每項在 `BEGIN … ROLLBACK` 裡跑 3 次（資料不變），兩次之間 VACUUM，表中是中位數（最小–最大），單位秒。原始資料：`results/ddl.tsv`。

| 操作 | 影響筆數 | A | B |
|---|---:|---:|---:|
| 刪冷門欄位 807 | 3,181 | 0.08（0.004–0.40） | 0.55（0.53–1.60） |
| 刪熱門欄位 3 | 950,123 | 18.2（4.5–27.8） | 206.7（173.9–258.1） |
| 冷門欄位 800 改型別 num → text | 3,256 | 0.03（0.03–0.22） | 0.56（0.56–0.72） |
| 熱門欄位 1 改型別 num → text | 949,336 | 18.5（16.9–47.1） | 195.8（180.4–211.6） |

- A 刪欄位是 `DELETE FROM issue_cf WHERE field_id = $f AND v_text IS NOT NULL`。**型別條件不能省**：第一次量時漏了，
  partial index 用不上、PK `(issue_id, field_id)` 的 field_id 又在第二欄，只能掃整張 `issue_cf`，冷門欄位也要 0.86 秒。
- B 刪欄位是 `UPDATE issues_b SET fields = fields - '$f' WHERE fields ? '$f'`。`jsonb_path_ops` 不支援 `?`，`@? '$."807"'` 也不走 GIN，
  所以連冷門欄位都要掃全表；熱門欄位等於把 95 萬筆 issue 整列重寫一次，所有索引跟著重寫，期間這些列都被鎖住。
- B 改型別時如果該欄位有 expression index（例如 `((fields->>'1')::numeric)`），還要另外 DROP／CREATE INDEX，表中沒算。
- A 處理熱門欄位的時間變異很大（4.5–47 秒），推測跟 checkpoint 與 WAL 寫入時機有關，沒有細查。

### 4.4 儲存大小（Q8）

灌完並 VACUUM 後（`results/sizes_base.tsv`、`sizes_covering.tsv`），單位 MB：

| | A 基本版 | A covering | B | C |
|---|---:|---:|---:|---:|
| issue 表（heap） | 156 | 156 | 514 | 514 |
| issue 表索引（pkey、可見性） | 46 | 46 | 46 | 46 |
| `issue_cf` heap | 1,005 | 1,005 | — | 1,005 |
| `issue_cf` pkey | 687 | 687 | — | 687 |
| 型別 partial index（5 個） | 236 | 822 | — | 822 |
| GIN | — | — | 242 | — |
| expression index（5 個） | — | — | 64 | — |
| **合計** | **2,130** | **2,715** | **865** | **3,073** |

A 最大的兩塊是 `issue_cf` heap（每列約 60 bytes，其中 tuple header 就佔 24）和 PK（`uuid` 16 bytes + `int`）。

## 5. 建議

### 5.1 Q9：選 A（型別化 EAV + covering partial index），不加 JSONB 快取

理由依重要性：

1. **欄位數量是使用者決定的，冷門欄位是常態。** 只要有人對一個沒有 expression index 的欄位篩選、排序或 group by，B 就是全表掃描；
   這在 100 萬筆是 100–300 ms，1000 萬筆會是秒級，而且會拖累其他查詢的快取。A 對每個欄位都是索引查詢，跟欄位多冷無關。
   B 唯一的補救是替每個會被篩選的欄位建 expression index，這在 1000 個欄位時不可行，還要處理型別轉換失敗與 DateStyle。
2. **寫入與維護。** A 的並行寫入吞吐量是 B 的 3.7 倍，膨脹約為 1/4.4，刪除或改型別熱門欄位快一個數量級，且不必改寫整列 issue。
3. **B 的優勢可以用別的方式拿到。** 在可見範圍上做 group by 與加總（B 快 1.2–3.6 倍）本來就規劃交給 tantivy 的 facet（§5.5）；
   熱門欄位排序快 2–3 倍是毫秒級差距。
4. **C 不划算。** 列表頁一次 50 筆只省約 1 ms，寫入吞吐量卻掉六成（8 clients，sync off：8,313 → 3,246 tps），還多一份要保持一致的資料。
   等真的量到顯示是瓶頸再加。

要接受的代價：儲存約為 B 的 3 倍（100 萬筆 2.7 GB vs 0.9 GB）；可見範圍上的多欄位組合 count 約 0.2 秒（B 0.3 秒），
「可見 + 熱門 3 欄」取第一頁 A 要 216 ms，B 3 ms（見 §5.3 第 3 點）。

### 5.2 M1 schema 草案

```sql
CREATE TABLE custom_fields (
    id          int GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name        text NOT NULL,
    ftype       text NOT NULL CHECK (ftype IN ('int', 'float', 'date', 'string', 'text', 'list', 'user', 'version', 'bool', 'link')),
    -- 其他屬性（is_required、is_filter、searchable、default、regexp …）依 M1 設計補
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE issue_cf (
    issue_id  uuid NOT NULL REFERENCES issues (id) ON DELETE CASCADE,
    field_id  int  NOT NULL REFERENCES custom_fields (id) ON DELETE CASCADE,
    v_date    date,
    v_bool    boolean,
    v_ref     uuid,      -- user / version / list 選項等參照
    v_num     numeric,   -- int 與 float
    v_text    text,      -- string / text / link
    PRIMARY KEY (issue_id, field_id),
    CHECK (num_nonnulls(v_num, v_date, v_text, v_ref, v_bool) = 1)
);
CREATE INDEX issue_cf_num  ON issue_cf (field_id, v_num,  issue_id) WHERE v_num  IS NOT NULL;
CREATE INDEX issue_cf_date ON issue_cf (field_id, v_date, issue_id) WHERE v_date IS NOT NULL;
CREATE INDEX issue_cf_text ON issue_cf (field_id, v_text, issue_id) WHERE v_text IS NOT NULL;
CREATE INDEX issue_cf_ref  ON issue_cf (field_id, v_ref,  issue_id) WHERE v_ref  IS NOT NULL;
CREATE INDEX issue_cf_bool ON issue_cf (field_id, v_bool, issue_id) WHERE v_bool IS NOT NULL;
```

和實驗版的差別（都是**沒量過**的設計判斷，M1 實作時可以順手驗證）：

- list 選項改存選項 id（`v_ref` 指向 `custom_field_options`），改選項名稱不必改寫值；實驗存的是選項文字。
- `text`（長文字）通常不用來篩選，可以考慮不進 `issue_cf_text`（例如另放一欄 `v_long text` 不建索引），讓索引只放短字串。
- 多值欄位（Redmine 的 `multiple`）在這個 PK 下放不進去，要嘛 PK 加上值或序號，要嘛另開一張表；實驗沒有多值欄位。

### 5.3 查詢產生器（Filter AST → SQL）要遵守的規則

1. **一定要帶型別條件**（`v_x IS NOT NULL` 或對 `v_x` 的比較），partial index 才用得上。漏掉時（例如 Q7 刪欄位）會變成全表掃描。
2. 每個欄位條件一個 `EXISTS` semijoin；排序用 `JOIN issue_cf c ON … AND c.field_id = $f AND c.v_x IS NOT NULL ORDER BY c.v_x`。
   實驗的排序是 inner join 語意（沒值的 issue 不列出）；Redmine 會把沒值的排在最後，要另外處理（例如先取有值的，不足 50 筆再補）。
3. planner 會低估列數：`field_id` 與 `v_x` 的統計是分開算的，`v_x` 的 null 比例（對某型別約 95%）又被重複計算，
   例如 `field_id = 8 AND v_num IS NOT NULL` 估 18 萬、實際 80 萬。這是 A 唯一明顯的規劃問題（例如「可見 + 熱門 3 欄」取第一頁選了先做 hash join，216 ms）。
   M1 可以試 `CREATE STATISTICS` 或依欄位覆蓋率決定 join 順序，這次沒測。
4. 在大集合上的 group by、加總、facet 交給 tantivy，與 §5.5 原規劃一致；SQL 版當備援。

## 6. 限制與沒測到的東西

### 6.1 量測本身

- **`shared_buffers` 128 MB 的影響很大。** 第一輪（A 基本版）時，A 的 bitmap heap scan 會讀過大半個 1 GB 的 `issue_cf`，
  把交錯執行的 B 頁面擠出 shared buffers，B 只好從 OS 快取重讀；同一個 B 查詢第一輪 180 ms、第二輪 12.5 ms，計畫完全相同、只差 `Buffers: hit`。
  所以 B 的數字一律取第二輪。實際部署 `shared_buffers` 會設大（數 GB），兩個方案都會更快，差距可能縮小或放大，這次沒量。
- 沒有冷快取（磁碟）測試；資料都在容器的 OS page cache 裡。
- 主機是一般桌機，有背景程式。第一次量（未交錯執行）時同一個查詢在兩輪之間差到 3–4 倍，改成各方案交錯執行後，中位數與最小值大多在 10% 內，
  但 p95 偶爾仍會跳（例如 Bx 熱門 date count 中位數 7.5、p95 118 ms）。結論只看差距在 2 倍以上的項目。
- 15 次取 p95 其實就是最大值，參考就好。
- JIT 開著（預設）；成本高的計畫會多 20–40 ms 編譯時間，兩方案都受影響。
- 寫入只測「改一個數字欄位」，沒測新增 issue（一次寫 18 個欄位）、批次匯入、同時改多個欄位。
- WAL 量被 full-page image 主導，看不出差異；沒量 checkpoint 之間的穩態 WAL。

### 6.2 沒測到的設計選項

- B 的其他變形：`jsonb_ops`（支援 `?`，但更大）、GIN `fastupdate=off`、用 `CREATE STATISTICS` 對 `fields->>'k'` 建統計、依欄位建 partial expression index。
- A 的其他變形：每個型別一張表（`issue_cf_num`、`issue_cf_date` …，可以消除 null 欄位與 null 比例造成的估計誤差）、
  用 bigint 代替 uuid 當 `issue_cf` 的 issue 鍵（PK 與索引會小約三成）、`CREATE STATISTICS`、`work_mem` 調大。
- 1000 萬筆以上、更多欄位、更寬的值（長文字）、多值欄位。
- 與 tantivy 的分工（facet 交給 tantivy 之後，SQL 端還剩哪些查詢）——要等 Filter AST 與 E6。
