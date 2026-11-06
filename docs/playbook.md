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

測試：`cargo test`。E4 可見性測試會在同一台 PostgreSQL 上自動建 `oxledger_test` 資料庫；也可以用 `OXLEDGER_TEST_DATABASE_URL` 指定。

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
| 搜「登入」出現「首頁圖片載入太慢」 | 索引有單字（unigram），查詢「登入」的「入」命中「載入」 | 查詢端：兩字以上的中文串只用 bigram，單字才用 unigram（`tokenize_query`）；索引仍存兩種。品質由 E1 驗證 |
| 用 Python 讀 MCP stdio 輸出時 JSONDecodeError | Windows 預設以 cp950 解碼管線 | `io.TextIOWrapper(sys.stdin.buffer, encoding="utf-8")` |
| Docker 指令連不到 `dockerDesktopLinuxEngine` | Docker Desktop 沒在跑，或剛啟動 engine 還沒好 | 啟動 Docker Desktop，等 `docker info` 有 ServerVersion（約 1 分鐘） |
| rustup 裝完 `cargo` 找不到 | 安裝時用了 `--no-modify-path` | 指令前加 `export PATH="$HOME/.cargo/bin:$PATH"`，或自行把 `%USERPROFILE%\.cargo\bin` 加進 PATH |

## 設計上的防呆

- **可見性只有一份規則**：`ox-core::Scope::predicate()` 產生 `Pred`，同時編成 SQL（`db::push_pred`）與 tantivy filter（`SearchIndex::filter`）。E4 測試拿 Redmine 規則的獨立實作對照四種結果。改規則時先跑 `cargo test -p oxledger --test visibility_e4`。
- **搜尋結果一律回 DB 複驗**：tantivy 可能落後資料庫（outbox 非同步），所以 `service::search` 會把候選 id 再過一次 SQL 可見性條件。
- **看不到 = 不存在**：`get` 對沒權限的 issue 回「no issue X」，和真的不存在一樣，避免用 key 探測。
- **outbox 用 xid8**：只消費 `xid < pg_snapshot_xmin(pg_current_snapshot())` 的列；長交易會拖住索引更新，之後要加監控。
