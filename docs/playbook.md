# oxledger playbook

開發環境怎麼起、踩過的坑（症狀 → 原因 → 修法）。邊做邊補。

## 本機開發環境

需要：Rust stable（1.99+）、Docker、PostgreSQL 18（用容器）。

```bash
# PostgreSQL 18，只綁 localhost，資料在 volume oxledger-pgdata
docker run -d --name oxledger-pg -e POSTGRES_USER=oxledger -e POSTGRES_PASSWORD=oxledger_dev \
  -e POSTGRES_DB=oxledger -p 127.0.0.1:55432:5432 -v oxledger-pgdata:/var/lib/postgresql postgres:18

cp .env.example .env            # 或直接 export 裡面的變數
cargo build
./target/debug/oxledger seed-demo   # 只能對空資料庫跑；印出 admin/alice/bob/carol 的 API key（只顯示這一次）
./target/debug/oxledger serve       # http://127.0.0.1:8080/mcp，Authorization: Bearer <key>
```

測試：`cargo test`。整合測試（E4 可見性、自訂欄位）預設用同一台 PostgreSQL 上的 `oxledger_test`；
`OXLEDGER_TEST_DATABASE_URL` 可以指到別的資料庫名稱，不存在會自動建（`tests/common/mod.rs`）。
多條工作線同時跑測試時各用各的，例如 `.../oxledger_test_d14`：測試會 TRUNCATE 整個 schema，共用會互相清掉資料。

### 接到 MCP client

- **HTTP（Streamable HTTP）**：URL `http://127.0.0.1:8080/mcp`，header `Authorization: Bearer <key>`。
- **stdio**：命令 `oxledger stdio`，環境變數 `DATABASE_URL`、`OXLEDGER_API_KEY`。log 走 stderr，不會污染 stdout。

示範資料的權限（用來手動驗證可見性）：

| 使用者 | AUTH（私有專案） | WEB（公開專案） |
|---|---|---|
| admin | 全部（管理員） | 全部 |
| alice | Manager（all） | 非成員 → Non member（default） |
| bob | 透過群組 Backend 拿 Developer（default） | Non member |
| carol | Reporter（own） | Non member |

AUTH-5 是私有單，只有 admin、alice 看得到。

示範的自訂欄位（D14）：Severity（list，Bug）、Estimate (h)（float）、Target date（date，Feature／Task）、
Customer（string，只在 AUTH；受限欄位，只有 Manager 也就是 alice 和 admin 看得到）、Reviewer（user）、Regression（bool，Bug）、Root cause（text，不建索引）。
`search` 的 `filter` 例子：

```json
{"and": [{"field": "Severity", "op": "in", "value": ["High", "Critical"]},
         {"or": [{"field": "Estimate (h)", "op": "between", "value": [2, null]},
                 {"field": "Reviewer", "op": "eq", "value": "me"}]}]}
```

op：`eq` `ne` `lt` `le` `gt` `ge` `between`（兩端含，任一端可為 null）`in` `is_null` `not_null`。
排序比較與 `between` 只限 int／float／date；text 只能 `is_null`／`not_null`。
語意照 Redmine（`Query#sql_for_custom_field`）：欄位只在「適用」的單上算數，也就是 tracker 在欄位的 tracker 清單、
專案是 all 或在清單內，而且呼叫者在該專案讀得到這個欄位。不適用的單上就算存了舊值也不算。
`ne` 是 `eq` 的補集，所以會包含沒有值與不適用的單；`is_null` 只算適用但沒有值的單。
因此 `is_null` 不等於 `not(not_null)`。讀不到的欄位會回「no custom field」，和不存在的欄位一樣。

## 症狀 → 原因 → 修法

| 症狀 | 原因 | 修法 |
|---|---|---|
| `QueryBuilder<'_, Postgres>`：struct takes 0 lifetime arguments | sqlx 0.9 拿掉了 QueryBuilder 的 lifetime 參數 | 寫 `QueryBuilder<Postgres>` |
| `sqlx::query_as(&sql)`：`SqlSafeStr` is not implemented for `&String` | sqlx 0.9 要求動態 SQL 明確標示 | 確認字串沒有拼入使用者輸入後，用 `sqlx::AssertSqlSafe(sql)`；值一律用 bind |
| `query_scalar(sql)` 參數是 `&str` 時 borrowed data escapes | 同上，`&'static str` 才算安全字串 | 參數型別寫 `&'static str` |
| `no RoleServer in rmcp::model` | rmcp 3.9 的 `RoleServer` 在 crate root | `use rmcp::RoleServer;` |
| `Uuid: JsonSchema` not satisfied | rmcp 帶的 schemars 只開 `chrono04` | 自己加 `schemars = { version = "1", features = ["uuid1"] }`，feature 會合併 |
| `field tool_router is never read` | `#[tool_handler]` 預設每次呼叫 `Self::tool_router()` 重建 | `#[tool_handler(router = self.tool_router, ...)]` |
| `cargo test` 報 failed to remove `target\debug\oxledger.exe` | Windows 上執行中的 exe 被鎖住 | 先停掉 `oxledger serve`／`stdio` 再編譯 |
| 搜「登入」出現「首頁圖片載入太慢」 | 索引有單字（unigram），查詢「登入」的「入」命中「載入」 | 查詢端：兩字以上的中文串用 bigram（權重 1，`tokenize_query`），串裡的單字另以 `CJK_UNIGRAM_WEIGHT` = 0.1 加入（D15，`query_terms`），讓「付不了錢」這類 bigram 全空的查詢仍有結果；索引仍存兩種。E1 實測 nDCG＠10 0.781 → 0.810，權重 0.5 以上「載入」誤命中會回來，不要調高（見 `docs/research/03-e1-zh-retrieval.md`；測試 `login_does_not_rank_loading_issue_high` 在 0.5 會失敗） |
| 搜 `invalid_grant` 時 `invalid_request` 排得一樣前面；搜 `v3.2.0` 命中任何含 `2`、`0` 的 issue | 舊斷詞把 `_`、`.` 當分隔符，識別碼只剩零件 | 索引端在零件之外另存整串（`_`、`.` 連接時），查詢端整串權重 1、零件 `COMPOUND_PART_WEIGHT` = 0.2。改了斷詞要**重建索引**（`oxledger reindex`）。細節與取捨見下方「識別碼斷詞」 |
| 用 Python 讀 MCP stdio 輸出時 JSONDecodeError | Windows 預設以 cp950 解碼管線 | `io.TextIOWrapper(sys.stdin.buffer, encoding="utf-8")` |
| Docker 指令連不到 `dockerDesktopLinuxEngine` | Docker Desktop 沒在跑，或剛啟動 engine 還沒好 | 啟動 Docker Desktop，等 `docker info` 有 ServerVersion（約 1 分鐘） |
| 自訂欄位篩選在 generic plan 下全表掃描 `issue_cf` | 每欄位的 partial index 條件是 `WHERE field_id = 5`；`field_id = $1` 在 prepared statement 換成 generic plan 後，planner 無法證明它蘊含索引條件 | field id 直接以整數字面值寫進 SQL（`filter::push_filter`）。`tests/custom_fields_pg.rs` 用 `EXPLAIN (GENERIC_PLAN)` 驗證兩種寫法 |
| `EXPLAIN (GENERIC_PLAN) … $1` 回 `bind message supplies 0 parameters` | sqlx 的 `query` 走 extended protocol，會要求綁值 | 用 `sqlx::raw_sql`（simple protocol） |
| `CREATE INDEX CONCURRENTLY cannot run inside a transaction block`（或在函式裡） | CONCURRENTLY 不能在交易或函式內 | migration 只放產生 DDL 的函式（`cf_index_ddl`、`cf_drop_index_ddl`），由 `custom_fields.rs` 在交易外用 `sqlx::raw_sql` 執行 |
| 欄位索引建失敗後重跑仍沒有可用的索引 | 失敗的 `CREATE INDEX CONCURRENTLY` 留下 INVALID 索引，`IF NOT EXISTS` 會直接略過 | `build_index` 先查 `pg_index.indisvalid`，INVALID 就先 `DROP INDEX CONCURRENTLY` 再建；`reconcile_indexes` 會定期補 |
| 本分支的 binary 跑過共用的 `oxledger` 之後，其他分支的 binary 啟動失敗（未實際觸發，依 sqlx `Migrator` 預設行為推斷） | 資料庫記錄了 migration 0002，舊分支找不到這個檔案就拒絕執行 | 開發中的 migration 用獨立資料庫試，例如 `DATABASE_URL=.../oxledger_d14_demo` |
| 改了已套用的 migration 後，測試或 binary 啟動失敗：`migration 2 was previously applied but has been modified` | sqlx 會比對每個已套用 migration 的 checksum | 還沒進 main 的 migration 可以改，但要重建自己的測試／示範資料庫（例如 `DROP DATABASE oxledger_test_d14`，`test_pool` 會自動重建）；進 main 之後一律另開新 migration |
| 合成資料用 `generate_series … CROSS JOIN fields WHERE random() < coverage`，結果每個欄位不是全有就是全無 | planner 把 `random()` 的過濾放在只算一次的位置（實測 200 個欄位只長出 12 個，每個 10 萬筆） | 用決定性的 hash 抽樣：`hashint8extended(i * 1000 + field_id, seed)`，E9 也是這樣做 |
| rustup 裝完 `cargo` 找不到 | 安裝時用了 `--no-modify-path` | 指令前加 `export PATH="$HOME/.cargo/bin:$PATH"`，或自行把 `%USERPROFILE%\.cargo\bin` 加進 PATH |

## 設計上的防呆

- **可見性只有一份規則**：`ox-core::Scope::predicate()` 產生 `Pred`，同時編成 SQL（`db::push_pred`）與 tantivy filter（`SearchIndex::filter`）。E4 測試拿 Redmine 規則的獨立實作對照四種結果。改規則時先跑 `cargo test -p oxledger --test visibility_e4`。
- **搜尋結果一律回 DB 複驗**：tantivy 可能落後資料庫（outbox 非同步），所以 `service::search` 會把候選 id 再過一次 SQL 可見性條件。
- **看不到 = 不存在**：`get` 對沒權限的 issue 回「no issue X」，和真的不存在一樣，避免用 key 探測。
- **自訂欄位（D14，型別化 EAV）**：值在 `issue_cf`，每種型別一個值欄位；觸發器 `issue_cf_check` 保證值放在該型別的欄位（`int` 必須是整數、list 必須是該欄位的選項、user 必須存在、string／link 最多 2000 bytes 以免超過 btree 單筆上限）。空字串不存（`set_value("")` 等於清除），對齊 Redmine 把 '' 當成沒有值。篩選器依賴這些保證：每個條件都是一個 `EXISTS (… cf.field_id = <id> …)`，帶型別條件，也就是對 `v_x` 的比較，或 is_null／not_null 時的 `v_x IS NOT NULL`（報告 02 §5.3、§8）。外面再包一層適用性條件 E（tracker、專案、可讀專案），由 `db::push_pred` 編譯。
- **受限欄位（`field_defs.visible = false` + `field_roles`）**：只有 admin，以及在該專案的實際角色包含指定角色的人讀得到。實際角色同 `db::role_ids_for`：成員角色（含透過群組）；不是成員的公開專案用內建 Non member；沒登入用 Anonymous。`field_roles` 可以放內建角色（migration 0003）。`search`（filter，`custom_fields::readable_projects`）和 `get`（custom_fields）用同一套規則；改其中一邊要同步改另一邊，`custom_fields_pg` 的 Triage 欄位會抓到不一致。
- **欄位不能改型別**：有觸發器擋，Redmine 也不允許。
- **list 選項**：使用中的選項不能刪（觸發器檢查，且只接受 READ COMMITTED，原因寫在 migration 註解），要停用就設 `active = false`；寫入值時對選項加 `FOR KEY SHARE`，和刪除互斥。
- **每個欄位一個 covering partial index** `issue_cf_f<id> (field_id, v_x, issue_id) WHERE field_id = <id>`。建立欄位時同步建（`create_field` 等索引建完才回傳），刪欄位走 `drop_field`：先標成 `dropping`。寫入觸發器會對欄位定義加 `FOR SHARE`，所以這個 UPDATE 會等正在寫值的交易結束，之後新的寫入一律被拒。接著經索引刪值 → `DROP INDEX CONCURRENTLY` → 刪定義。`serve` 每 60 秒跑 `reconcile_indexes` 收拾中斷的建立或刪除。
- **`issue_cf.field_id` 沒有 FK**：刪欄位時 FK 檢查要用 field_id 查 `issue_cf`，只有該欄位的 partial index 能用，而它可能已經刪掉。改由觸發器檢查存在性並加鎖，`drop_field` 先刪值。
- **數字一律綁成 text 再 `::numeric`**：綁 `f64` 會變成 `numeric > float8`，PostgreSQL 會把欄位轉成 float8，索引就用不上（依型別規則推斷，沒實測）。
- **planner 低估列數：部分處理**（報告 02 §8 有實測數字）。比較條件不再多帶 `IS NOT NULL`，熱門數字範圍的低估從 22 倍降到 4.9 倍；`issue_cf_field_ref` 統計讓 list／user 等值從 30 倍修到約 1 倍。剩下的是數字和日期範圍的低估、is_null／not_null 的 4.75 倍低估，以及跨欄位或 join 的相關性。多個熱門欄位加 `LIMIT` 時的 join 順序沒有重測。
- **free text + 篩選**：tantivy 不知道自訂欄位、狀態和 assignee=none，所以 `q` 的候選從 500 開始，存活數不夠就擴到 2000、8000。到 8000 還不夠時回 `truncated: true`，表示可能還有更多。每輪都重新查索引，索引在查詢期間更新時排序可能變動（best effort）。
- **outbox 用 xid8**：只消費 `xid < pg_snapshot_xmin(pg_current_snapshot())` 的列；長交易會拖住索引更新，之後要加監控。

## 識別碼斷詞（2026-11-06，和 GPT 討論 2 輪後實作）

起因：E1 報告 §6 第 5 點，`_`、`.` 把錯誤碼和版本號拆散。規則寫在 `search.rs` 的 `Lexer`。

**規則**
- 非 CJK 的英數字串（Unicode `is_alphanumeric`，和原本的詞一樣）之間只隔一個 `_` 或 `.` 時，除了各段，再存一個完整的詞：`invalid_grant` → `invalid`、`grant`、`invalid_grant`；`v3.2.0` → `v3`、`2`、`0`、`v3.2.0`。
- 只取最長的那一串，不產生中間組合（`foo_bar.baz` 只多一個 `foo_bar.baz`）。
- 不會連接的情況：
  - 結尾的連接符：`invalid_grant.` 只產生 `invalid_grant`。
  - 開頭的連接符，或連續兩個連接符：`__init__` → `init`；`a..b`、`a._b` → `a`、`b`。
  - 中文是硬邊界：`登入.失敗` 不連接。
- 整串超過 128 bytes 時不存整串，各段當一般的詞。
- `-`、`/`、`:`、`@`、全形破折號仍是分隔符。
- 查詢端權重：整串 1、各段 0.2。各段的 0.2 沒有校準過，E1 的查詢集裡沒有識別碼查詢。

**討論中決定的事**
- `-` 先不連接，因為 `登入-失敗` 這類中文混寫，以及日期的語意都要另外想。`/`、`:`、`@`（路徑、URL、時間、email）也延後，等有實際需求再說。
- 版本號不特別處理（GPT 第 1 輪建議版本號只搜整串、不搜各段，第 2 輪改成和其他複合詞一樣）。代價是：
  - 搜 `v3.2` 而 issue 寫 `v3.2.0`，或搜 `3.2.0` 而 issue 寫 `v3.2.0` 時，沒有整串可以對上，只能靠 `v3`、`2`、`0` 這些零件，所以 `v3.9.2` 也可能排上來。這只能找回大致相關的結果，不懂版本前綴。
  - 之後若版本號查詢重要，做法是在索引端加有上限的前綴和去掉 `v` 的別名（`v3.2`、`3.2.0`），並寫專門的測試。
  - 「零命中才退回搜零件」不採用：只要有一筆不相關的命中，退回就不會觸發。
- 段落用 Unicode 英數字，不限 ASCII，和原本的詞一致；但不做正規化，全形 `ｖ３.２` 不等於 `v3.2`。
- 驗收：E1 的 nDCG＠10 不能低於 0.810，同時逐題看變化，並看索引大小。

**結果**
- E1（只跑 a、a2、b、c）：a 0.781、a+uni0.1 0.810，到小數第 3 位都沒變，recall／MRR／P＠10 也一樣。60 題裡只有 q01 降了 0.002。
- 索引：200 筆時 103.1 → 103.8 KB；10,000 筆時 1869.5 → 1880.4 KB（+0.6%）。
- E1 沒有識別碼查詢，所以精確度的提升只由測試證明：
  - `identifiers_glued_by_underscore_or_dot_are_kept_whole`：斷詞的邊界情況。
  - `exact_identifier_ranks_first`：只有零件命中的 issue，分數低於第一名的 0.15 倍；各段權重若改成 1.0，這個比例約 0.3，測試會失敗。
- 還沒做：真實或獨立撰寫的識別碼查詢集，包括版本號、相近錯誤碼、路徑、日期和中文邊界的反例。
