# 06 可觀測性（observability）研究

> 狀態：研究稿，2026-11-13。只讀 repo、查公開資料，沒有下載或安裝任何東西，沒有改程式。
> 範圍：oxledger 後端（`crates/oxledger`）、背景 worker、SPA（`web/`）、本機開發與 E2E。

---

## 0. 一頁結論

- **現況**：有 `tracing` 文字 log（stderr）、一個手寫 Prometheus 格式的 `/metrics`（只涵蓋搜尋同步、notify、長交易），`/healthz` 是靜態 `ok`。**沒有 span、沒有 HTTP 指標、沒有連線池指標、沒有真正的 request id**——`meta.request_id` 每次回應都新產一個 UUID，和 log 沒有關聯，只有 500 會把 id 寫進 log。
- **M2 最小集合（建議都不引入新的 crate）**：
  1. 每個請求一個 request id 與一個 `tracing` span（同一個 id 出現在回應標頭、`meta`／`error` 本文、這個請求的每一行 log）。
  2. HTTP RED 指標（依 route 樣板、不含 query string）。
  3. 連線池指標＋把現有的 `RELEASED_IN_TRANSACTION` 匯出＋設定 `application_name`。
  4. 所有背景 worker 用同一組指標（rounds、last success、backlog、oldest pending age），補上 GitHub 與 AI 兩個 worker。
  5. `/readyz`（DB＋worker 狀態，不洩漏細節）。
  6. E2E 失敗時自動附上後端 log 片段、`/metrics` 快照與 worker 狀態。
  7. 把新增的 log 欄位納入既有的「hidden content stays out of logs」測試。
- **之後再做**：OpenTelemetry traces（Rust 的 traces 仍是 Beta）＋自架 Jaeger v2 或 Tempo、前端 `traceparent`、集中 log（Loki）、SLO burn-rate 告警。
- **不建議**：任何把 log／trace 送到外部 SaaS 的預設方案；Apache SkyWalking（起源與主要維護圈在中國，見 §4）。

---

## 1. 現況盤點

### 1.1 Logs

| 項目 | 現況 | 位置 |
|---|---|---|
| 框架 | `tracing` 0.1.44 + `tracing-subscriber` 0.3.23，只開 `env-filter` feature | `crates/oxledger/Cargo.toml` |
| 初始化 | `fmt()` 文字格式、寫到 stderr（為了讓 stdio MCP 的 stdout 乾淨）、`RUST_LOG` 預設 `info` | `src/main.rs` `main()` |
| JSON 格式 | **沒有**（`json` feature 沒開） | — |
| span | **全 repo 0 個** `#[instrument]`／`info_span!` | `grep` 結果 |
| 呼叫點 | `src/` 下 17 個檔案、約 62 處 `tracing::` 事件，多半是 worker 失敗的 `warn!`／`error!` | — |
| 請求 log | **沒有 access log**。domain error（4xx）完全不記；非 domain error 的 500 記一行 `request failed`，帶 `request_id` | `src/api.rs` `impl IntoResponse for ApiFailure` |
| sqlx 查詢 log | sqlx 0.9 預設：每條語句在 `sqlx::query` target 用 DEBUG 記（含 SQL 文字，不含 bind 值），超過 1 秒用 WARN 記 `slow statement`；`RUST_LOG=info` 時只看得到慢查詢 | sqlx-core 0.9.0 `src/logger.rs`、`src/connection.rs` |
| 連線池 log | sqlx 預設：取得連線超過 2 秒在 `sqlx::pool::acquire` 用 WARN 記；G20 的歸還檢查攔到交易中的連線時 `warn!` 一行 | sqlx-core `pool/inner.rs`、`src/db.rs` |
| E2E 時的後端 log | `web/e2e/backend.mjs` 預設 `RUST_LOG=warn`；`playwright.config.ts` 的 webServer `stdout: 'ignore'`（stderr 仍會印到終端機，但不會存檔、不會附在失敗報告上） | `web/e2e/backend.mjs`、`web/playwright.config.ts` |

### 1.2 Metrics

`src/metrics.rs`（手寫 Prometheus text format 0.0.4，沒有 metrics crate），`GET /metrics` 只在設了 `OXLEDGER_METRICS_TOKEN` 時掛上，要 `Authorization: Bearer`，用 SHA-256 摘要比對。

| 群組 | 指標 |
|---|---|
| 搜尋同步 | `search_index_lag_seconds{bound=upper\|commit}`（histogram）、`search_consistency_wait_seconds`、`search_consistency_outcome_total{outcome}`、`search_sync_rounds_total{result}`、`search_generation_switches_total`、`search_outbox_pending`、`search_outbox_oldest_pending_age_seconds`、`search_frontier_stalled_seconds`、`search_sync_last_success_timestamp_seconds`、`search_frontier` |
| PostgreSQL | `pg_oldest_xid_holder_age_seconds`；另有每 10 秒的長交易檢查（15 秒 warn、60 秒 error，寫 log，不殺連線） |
| notify | `notify_dispatch_delay_seconds`、`notify_rounds_total{result}`、`notify_events_total`、`notify_deliveries_total`、`notify_skipped_issues_total`、`notify_last_success_timestamp_seconds`、`notify_outbox_pending`、`notify_outbox_oldest_pending_age_seconds` |

**缺的**：

- HTTP：請求數、錯誤率、延遲（RED 三項全缺），`/api/v1` 與 `/mcp` 都沒有。
- 連線池：`size`、`idle`、取得等待時間全缺。`db::RELEASED_IN_TRANSACTION`（G20 的計數器）**存在但沒有匯出到 `/metrics`**，只有測試 `tests/p2_tx_cancel.rs` 讀它。
- worker：GitHub worker（`github::spawn_worker`、`github_event_jobs.attempts`）、AI runner（`ai::runner::spawn_jobs`、`ai_runs.lease_until`）、custom field 索引 job、login challenge sweeper **都沒有指標**，只有失敗時的 `warn!`。這正是「worker 落後時看不出卡在哪」的原因：搜尋與 notify 有 backlog 指標，其他沒有。
- process：記憶體、FD、tokio runtime 都沒有（優先度低）。

### 1.3 Traces 與 correlation

- **沒有 trace**：沒有 span，也就沒有跨 await 的上下文；背景 worker 的一輪（round）也沒有 span。
- **request id 是假的**：`api.rs` 的 `ok()` 每次回應呼叫 `Uuid::now_v7()` 放進 `meta.request_id`；`ApiFailure::into_response` 又另外產一個。同一個請求的 log 行（若有）和回應裡的 id **沒有關係**，只有 500 那一行對得上。沒有 `X-Request-Id` 回應標頭，也不接受進來的 id 或 `traceparent`。
- **前端**：`web/src/lib/api.ts` 送 `Idempotency-Key`（寫入類）與 `X-CSRF-Token`，不送任何 trace／request 標頭；錯誤本文有 `request_id` 欄位。
- **DB 端無法對應**：pool 沒設 `application_name`，所以 `pg_stat_activity` 與 `xid_holders()` 的長交易警告裡看不出是 `serve` 還是 `stdio`、哪個 worker。

### 1.4 Health

- `/healthz`：靜態回 `"ok"`，不碰 DB，等於 liveness。E2E 的 webServer 用它判斷後端起好了。
- 沒有 readiness：DB 斷線、migration 沒跑完、搜尋同步停滯時，`/healthz` 照樣 `ok`。搜尋停滯目前只反映在搜尋 API 回 `search_unavailable`（`SearchMetrics::sync_stalled`，60 秒無成功輪次）。

### 1.5 稽核與隱私（已經做得好的地方）

- 稽核資料在 DB：`auth_events`、各實體的 `*_events`＋`*_event_redactions`（issue、version、project_setting、view_def、time_entry）、`agent_write_log`、`ai_provider_events`、`ai_usage_daily`。**稽核不走 tracing**，這個分層是對的，建議保持。
- 測試 `crates/oxledger/tests/s8_gate_extra.rs::hidden_content_stays_out_of_logs_and_audit`（E4w-7）：在 TRACE 等級、所有 crate 都開的 subscriber 下，讓看不到私有 issue 的 alice 和她的 agent 去讀、搜尋、提交 command，斷言 log、錯誤訊息、`auth_events.details` 都不含秘密字串。**這是新增任何 log 欄位時的守門測試**，§3 的每一步都要讓它繼續涵蓋。
- AI 設定的 log 只寫 endpoint 名稱與原因，不寫 URL 或 key（`main.rs` `serve()` 的註解與實作）。

### 1.6 對照使用者提到的除錯痛點

| 痛點 | 為什麼難查 | 哪個最小集合項目能縮短時間 |
|---|---|---|
| G20：連線池借出還在交易中的連線，偶發 500 | 500 的 log 只有錯誤字串；看不到是哪條 route、前一個借用者是誰、池裡是否常發生 | request span（route＋request id）、`db_pool_released_in_transaction_total` 匯出、`application_name`、慢 acquire log 門檻 |
| G35 | **在 docs 裡沒找到以「G35」為編號的連線池問題**：`docs/impl/p1-backend-gaps.md` 只有 G20；`T-G35` 是 GitHub ingest 的測試編號（`s6b-checklist.md`）。請使用者確認指的是哪一筆 | — |
| worker 落後看不出卡在哪 | 只有搜尋與 notify 有 backlog；GitHub／AI／field job 沒有；沒有「這一輪花多久、卡在哪一步」 | 統一 worker 指標＋每輪一個 span（帶 worker 名、輪次耗時、處理筆數） |
| E2E 失敗只看到逾時 | 後端 log 是 `warn` 等級且不存檔；Playwright trace 只看得到前端 | E2E 失敗自動附件（後端 log 片段、`/metrics`、worker 狀態），回應標頭帶 request id 讓 Playwright trace 能對到後端 log |

---

## 2. 業界作法（附來源）

### 2.1 OpenTelemetry 的 Rust 生態

- **opentelemetry-rust 狀態**（官方 README 的狀態表）：Logs API／SDK／OTLP exporter／`opentelemetry-appender-tracing` 為 **Stable**；Metrics API／SDK／OTLP exporter 為 **Stable**，Prometheus exporter 為 **Beta**；**Traces API／SDK／OTLP exporter 仍是 Beta**。opentelemetry.io 的 Rust 頁同樣標示 Traces Beta、Metrics Stable、Logs Stable。
- 社群會議記錄顯示 traces 穩定化時程已延後（原訂 2026-03 的 Tracing API stable 延到之後，主因是效能問題可能要改 API）；release line 在 0.32（2026-05-08）附近。**這些是非正式紀錄**，導入前要再看當時的 release notes。
- **`tracing-opentelemetry`**（tokio-rs 組織維護）：把 `tracing` 的 span 轉成 OTel span 的 layer。README 說它的版本「比 opentelemetry 快一號」（例如 0.26 對 opentelemetry 0.25），且不保證一直如此——**版本要成對升級**，這是導入的主要維護成本。
- 官方 README 建議新專案用 `tracing` 記 log，再用 `opentelemetry-appender-tracing` 橋接到 OTel logs。對 oxledger 來說，**現有的 `tracing` 呼叫點不用改**，之後要接 OTel 只是多掛 layer。

結論：M2 不需要 OTel；先把 `tracing` 的 span 結構做對（M2 就有用：log 會帶上 span 欄位），之後接 OTel 時零改寫。

### 2.2 三大支柱與方法論

- **Structured logs**：每行一個 JSON 物件，欄位固定（時間、等級、target、message、span 欄位如 `request_id`、`route`）。`tracing-subscriber` 的 `json` feature 就能做（同一個 crate，開 feature 即可）。
- **Metrics — RED**（Tom Wilkie 提出，Grafana Labs 部落格整理）：面向服務的 Rate、Errors、Duration。適用 `/api/v1`、`/mcp`、每個 worker 的「輪次」。
- **Metrics — USE**（Brendan Gregg）：面向資源的 Utilization、Saturation、Errors。適用連線池（使用中／上限、等待取得的時間、取得逾時與交易中歸還）、outbox backlog、tantivy writer。
- **Distributed tracing**：一個請求跨元件的因果鏈。oxledger 目前是單一程序＋PostgreSQL，跨程序只有「SPA → 後端」與「後端 → GitHub／AI provider」，所以 trace 的價值主要在**同一程序內**：一個請求花在 acquire、哪條查詢、哪個 tantivy 操作。這用 `tracing` span 加上 log 就能拿到大半，不一定要 trace 後端。

### 2.3 前後端串接：W3C Trace Context

- W3C Trace Context 是 **W3C Recommendation（2021-11-23）**。`traceparent` 格式 `version-trace_id-parent_id-flags`，例：`00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01`；trace-id 16 bytes、parent-id 8 bytes，皆為小寫 hex，全 0 無效。
- OTel JS 的 traces／metrics 是 Stable，但**瀏覽器端的 instrumentation 標示為 experimental**（opentelemetry.io JS 頁）。
- 對 oxledger 的含意：前端不必引入 OTel JS，可以在 `api.ts` 的 `fetcher` 自己產生 `traceparent`（幾行程式、不需新套件），後端在 request span 裡記下 trace-id。等後端接 OTel 時，這個 header 自然成為 parent。

### 2.4 sqlx 查詢與連線池

（以本機 cargo registry 裡的 sqlx-core 0.9.0 原始碼為準，見 §6 踩坑）

- **查詢 log**：`ConnectOptions::log_statements(level)`、`log_slow_statements(level, duration)`、`disable_statement_logging()`；預設一般語句 DEBUG、慢語句（≥1 秒）WARN，target `sqlx::query`，欄位有 `summary`、`db.statement`、`rows_affected`、`rows_returned`、`elapsed_secs`。**SQL 文字會進 log，但 bind 參數不會**——所以只要不把內容用 `format!` 拼進 SQL，就不會洩漏（測試 helper `exec` 用 `format!`，但那是測試碼）。
- **連線池**：`Pool::size()`、`Pool::num_idle()` 可在 scrape 時讀；**sqlx 沒有提供取得等待時間的指標 API**，只有 `acquire_time_level`（預設 Off）與 `acquire_slow_level`／`acquire_slow_threshold`（預設 WARN、2 秒）兩個 log 開關，target `sqlx::pool::acquire`，欄位 `acquired_after_secs`。要等待時間的 histogram，得在自己的 `acquire()`／`begin()` 包一層計時（oxledger 有 `service::read_tx` 這類集中入口，可在那裡做）。
- 其他預設：`max_connections` 10、`acquire_timeout` 30 秒、`test_before_acquire` true。**oxledger 的 `serve` 有 5 個以上背景 loop 與所有請求共用同一個 10 條連線的池**，池滿時請求會排隊；沒有指標就看不出來。
- **交易中歸還**：sqlx 沒有內建計數，oxledger 的 `after_release` 檢查（G20 修補）已經有 `RELEASED_IN_TRANSACTION`，匯出即可。
- **application_name**：PostgreSQL 標準連線參數，`PgConnectOptions` 可設；設了之後 `pg_stat_activity`、長交易警告、PG 自己的 log（`%a`）都能分辨來源。

### 2.5 背景 worker

業界通用的 worker／queue 指標（Prometheus 命名慣例：counter 用 `_total`、時間用 `_seconds`、單位放名稱尾）：

- `*_rounds_total{result}`、`*_round_duration_seconds`（histogram）
- `*_last_success_timestamp_seconds`（告警用 `time() - x > 門檻`，比「失敗次數」更能抓到「整個 loop 停了」）
- backlog：`*_pending`、`*_oldest_pending_age_seconds`（佇列年齡比數量更能代表使用者感受到的延遲）
- 錯誤分類：`*_errors_total{kind}`（kind 是固定列舉，不放錯誤字串）

oxledger 的搜尋與 notify 已經是這個形狀；建議把它抽成 `worker="search|notify|github|ai|field_index|sweeper"` 的同一組名字，新 worker 照抄。

### 2.6 health／readiness、SLO 與告警

- Kubernetes 的慣例：liveness（程序還活著，失敗就重啟）與 readiness（能不能接流量，失敗就摘掉）分開；readiness 不該因為下游短暫抖動就讓 liveness 失敗，否則會重啟風暴。即使 oxledger 不跑在 k8s，這個區分對反向代理與部署腳本一樣適用。
- **SLO 告警**：Google SRE Workbook〈Alerting on SLOs〉建議多窗口多 burn-rate：以 99.9% SLO 為例，1 小時＋5 分鐘窗口 burn rate 14.4 倍（耗掉 2% 預算）page、6 小時＋30 分鐘 6 倍（5%）page、3 天＋6 小時 1 倍（10%）開 ticket。短窗口約為長窗口 1/12，讓告警在問題結束後很快解除。
- 對公司內部、使用者數十人等級的服務，**M2 不需要正式 SLO**；先有 RED 指標與 worker `last_success`，加幾條簡單告警（5xx 比率、worker 停滯、長交易、池等待）就足夠。

### 2.7 稽核 log 與一般 log 分開；隱私

- 稽核（誰在何時做了什麼、可追責、要保存）留在 DB 的 append-only 事件表，有 redaction 機制；一般 log（給工程師除錯、可隨時丟）走 `tracing`。兩者**不混用**：不要把稽核事件也 `info!` 一份到 log（會讓 redaction 失效——DB 裡遮掉了，log 檔裡還在）。
- 一般 log 的欄位白名單原則：
  - 只記 **id 與列舉**（issue id、project id、route 樣板、錯誤 code），**不記內容**（subject、description、note、搜尋字串、custom field 值、AI prompt／回應）。
  - **不記原始 URI**：`/api/v1/search?q=...` 的 query string 會帶搜尋字串；request span 只記 `MatchedPath`（route 樣板）。現成的 `tower-http` `TraceLayer` 預設 span 會帶完整 URI，這是不直接套用它的理由之一。
  - 錯誤訊息：domain error 的 message 已經經過 E4w-7 測試；500 的 `{:#}` 會印整條 anyhow chain，sqlx 的錯誤可能帶 PG 的 `DETAIL`（例如 unique 違反時的鍵值）。建議 500 log 保留，但納入 E4w-7 類型的測試情境。
- metrics 的 label 也是 log 的一種：label 只能是有限列舉（route、status class、worker、result），不能是 user、issue、搜尋字串——同時避免 cardinality 爆炸。

### 2.8 本機開發與測試

- Playwright 已設 `trace: 'retain-on-failure'`，前端的網路請求與回應標頭會在 trace 裡。只要後端在回應標頭放 `X-Request-Id`，失敗時就能拿 id 去後端 log 搜尋。
- 做法：
  1. `backend.mjs` 把後端 stderr 寫到 `web/test-results/backend-<run>.log`（而不是 ignore）；E2E 時 `RUST_LOG` 用 `info,oxledger=debug`，配 JSON 格式方便過濾。
  2. Playwright `afterEach`：測試失敗時 `testInfo.attach` 後端 log 中這個測試時間窗的片段、`/metrics` 快照（E2E 自帶 token）、worker 狀態。
  3. Rust 整合測試：失敗時 dump 目前 worker 狀態與池狀態（`size`／`num_idle`／`RELEASED_IN_TRANSACTION`），可以用 test-support 裡的 helper 實作。
- 這一項成本低、回報高，而且完全在本機，不涉及部署。

---

## 3. 方案比較

### 3.1 程式內（instrumentation）

| 方案 | 內容 | 導入成本 | 執行期成本 | 風險 |
|---|---|---|---|---|
| **I-1 request span＋request id（手寫 middleware）** | 一個 axum `from_fn` middleware：產生 UUIDv7（或從 `traceparent` 取 trace-id）、開 `info_span!("http", request_id, method, route, status, latency_ms)`、放進 request extension、回應加 `X-Request-Id`；`ok()`／`ApiFailure` 改讀 extension 而不是自產 | 小（約 100–200 行＋改 `ok()` 的呼叫方式；`ok()` 呼叫點多，需要用 task-local 或 extension 傳遞） | 每請求一個 span，可忽略 | `meta.request_id` 的語意改變（現在每次新產）——契約 02 §4.3 要確認；hidden-content 測試要涵蓋 span 欄位 |
| I-1' 改用 `tower-http` `TraceLayer`＋`SetRequestIdLayer` | 現成層 | 小，但**新增依賴**（tokio-rs 維護，非中國） | 同上 | 預設 span 帶完整 URI（含 query string，會帶搜尋字串）；要自訂 `make_span_with` 才安全 |
| **I-2 JSON log** | 開 `tracing-subscriber` 的 `json` feature，`OXLEDGER_LOG_FORMAT=json` 切換 | 極小（開 feature＋幾行） | 可忽略 | 只是格式；Cargo feature 變更會帶入 `tracing-serde` 等相依，算「新增依賴」需使用者確認 |
| **I-3 HTTP RED 指標（手寫，延續 metrics.rs）** | `http_requests_total{route,method,status_class}`、`http_request_duration_seconds{route}`；`/mcp` 用 `route="/mcp"`＋`tool` label（tool 名是有限列舉） | 小（`Histogram` 已有，加一個以 route 為 key 的 map） | 每請求一次 lock＋原子加；可接受 | route label 必須用 `MatchedPath`，否則 cardinality 爆 |
| I-3' 改用 `prometheus-client`（Prometheus 官方 org）或 `metrics`＋`metrics-exporter-prometheus` | 現成型別、OpenMetrics | 中（新依賴，把現有手寫的搬過去） | 相近 | 手寫已經能用；換庫的收益在 exemplar、OpenMetrics，M2 用不到 |
| **I-4 連線池指標** | scrape 時讀 `size()`、`num_idle()`、`max_connections`；匯出 `RELEASED_IN_TRANSACTION`；在集中的 acquire／begin 入口計時成 histogram；`application_name` 設成 `oxledger serve`／`oxledger stdio`；`acquire_slow_threshold` 調到 500 ms | 小 | 計時可忽略 | acquire 等待時間只量得到「經過我們入口」的部分 |
| **I-5 worker 統一指標＋每輪 span** | 見 §2.5；GitHub 補 `github_jobs_pending`、`github_jobs_oldest_pending_age_seconds`、`github_job_attempts` 分布；AI 補 `ai_runs{state}`、過期 lease 數 | 中（4 個 worker 各改一些，搜尋／notify 改名或加 alias） | scrape 時多幾條 `count(*)`，有索引就便宜 | 改名會讓已有的 dashboard／告警失效（目前應該還沒有）|
| **I-6 `/readyz`** | 檢查 DB `SELECT 1`（短逾時）、migration 版本、各 worker `last_success` 未超過門檻；只回 200／503＋固定代碼，細節只在 `/metrics` | 小 | 每次探測一個查詢 | 不能讓 readiness 因單一 worker 停滯就摘掉整個服務——worker 停滯應是告警而不是 not ready（要決定，見 A 類問題） |
| **I-7 E2E／測試失敗附件** | §2.8 | 小到中（JS 為主） | 只在測試 | 附件可能含 fixture 資料；fixture 是假資料，沒有公司資料問題 |
| I-8 OTel traces | `tracing-opentelemetry`＋`opentelemetry-otlp` 送到自架 collector／Jaeger／Tempo | 中（4 個新 crate、版本要成對；exporter 背景 task、shutdown flush） | batch exporter 有額外記憶體與 CPU；可取樣 | Rust traces 仍 Beta，API 可能再變；**送到哪裡是公司資料問題**，只能送內部 |
| I-9 前端 `traceparent` | `api.ts` 自產 header；或用 OTel JS web SDK | 自產：極小；OTel JS：中 | 可忽略 | OTel 瀏覽器端仍 experimental |
| I-10 PG 端 | `pg_stat_statements`、`auto_explain`、`log_min_duration_statement` | 小（DB 設定，屬部署） | 小 | 改 PG 設定屬環境變更，要部署方同意；`auto_explain` 可能把參數值寫進 PG log（`log_parameter_max_length` 要設 0） |

### 3.2 收集與檢視後端（都能自架、離線）

| 工具 | 用途 | 維護者／所在地 | 授權 | 單機部署 | 適合度 |
|---|---|---|---|---|---|
| **Prometheus** | metrics 收集、查詢、告警規則；Alertmanager 發告警 | CNCF 畢業專案，社群治理；起源於 SoundCloud（德國柏林） | Apache-2.0 | 單一 binary，本機 TSDB | **高**：oxledger 已輸出 Prometheus 格式 |
| **Grafana** | 儀表板，可接 Prometheus／Loki／Tempo／Jaeger | Grafana Labs（美國公司；總部所在地本次未直接查證，公開資料常記為紐約） | AGPL-3.0（2021-04 起；外掛、agent 部分仍 Apache-2.0） | 單一 binary＋SQLite | 高；AGPL 對「內部自用、不改 Grafana 原始碼」無額外義務，但要讓法務知道 |
| **Grafana Loki** | log 收集與查詢（依 label 索引，不全文索引） | Grafana Labs | AGPL-3.0 | 單一 binary 模式可行 | 中：使用者數少時，JSON log 檔＋`jq`／`grep` 也夠；多台或要長期保存時再上 |
| **Grafana Tempo** | trace 儲存（物件儲存或本機磁碟） | Grafana Labs | AGPL-3.0 | 單一 binary 模式可行 | 中：和 Grafana 整合最好 |
| **Jaeger v2** | trace 收集與 UI；v2（2024-11）建在 OpenTelemetry Collector 上，原生收 OTLP | CNCF 畢業專案；起源於 Uber（美國） | Apache-2.0 | 單一 binary，Badger（內嵌）或記憶體儲存；另支援 Cassandra、Elasticsearch／OpenSearch、ClickHouse、Kafka | **高**（trace 階段）：最省事的離線 trace UI |
| OpenTelemetry Collector | 中繼、取樣、轉送、遮罩欄位 | CNCF | Apache-2.0 | 單一 binary | 中：只有一個服務時可以不用；要多後端或要在送出前做欄位遮罩時有用 |
| SigNoz | metrics＋traces＋logs 一體，OTel 原生，後端 ClickHouse | SigNoz Inc.（YC W21；登記地多記為美國舊金山，團隊主要在印度；創辦人 Pranay Prateek、Ankit Nayan） | 核心 MIT，另有企業版目錄 | 需 ClickHouse＋多個容器，較重 | 中低：功能全，但對單一服務過重；ClickHouse 由 ClickHouse Inc.（美國）維護，起源於 Yandex（俄羅斯），非中國 |
| OpenObserve | logs／metrics／traces 一體，Rust 寫的 | OpenObserve Inc.（前身 ZincSearch），公開資料記為美國舊金山、創辦人 Prabhat Sharma，團隊遠端 | AGPL-3.0 | 單一 binary | 中低：查到的資料多來自商業資料庫與廠商自己的網站，來源薄弱；**沒有找到中國維護者的證據**，但也沒有足夠證據完全排除，若要採用需再查證 |
| ~~Apache SkyWalking~~ | APM | Apache 軟體基金會專案；創辦人吳晟（Wu Sheng）2015 年在中國起始，曾任職華為，華為 DevCloud 曾支持此專案；主要貢獻者圈以中國為主（本次沒查到 PMC 國別統計） | Apache-2.0 | — | **排除**：依「不用中國產品或中國維護者」的限制 |
| （SaaS：Grafana Cloud、Datadog、Honeycomb 等） | — | 外部服務 | — | — | **不作預設**：公司資料只能用公司帳號處理，log／trace 含公司資料，送外部 SaaS 需公司層級核准 |

---

## 4. 建議：最小集合與分階段導入

### 階段 0（M2 期間，建議都做；不新增 crate，除了 A1 的 `json` feature）

順序依「對已知痛點的回報／成本」排：

1. **request id＋request span（I-1）**：同一個 id 出現在 `X-Request-Id` 標頭、`meta.request_id`／`error.request_id`、這個請求所有 log 行。route 用樣板。`/mcp` 也套（tool 名進 span）。
   - 驗收：一個 500 的回應 id 在 log 裡搜得到，且同一行帶 route；hidden-content 測試在 span 欄位下仍綠。
2. **池指標＋`application_name`＋匯出 G20 計數（I-4）**：`db_pool_connections{state="idle|in_use"}`、`db_pool_max_connections`、`db_pool_acquire_seconds`、`db_pool_released_in_transaction_total`；`acquire_slow_threshold` 500 ms。
   - 驗收：`tests/p2_tx_cancel.rs` 的情境之後 `/metrics` 看得到計數增加；`pg_stat_activity` 看得到 `oxledger serve`。
3. **HTTP RED（I-3）**。
4. **worker 統一指標＋每輪 span（I-5）**：先補 GitHub 與 AI（目前全盲），搜尋／notify 保留現名、加 `worker` 名的通用名字。
5. **E2E／測試失敗附件（I-7）**：後端 log 存檔、失敗時附 log 片段＋`/metrics`＋worker 狀態。
6. **`/readyz`（I-6）**。
7. **JSON log 開關（I-2）**。

每一步都擴充 `hidden_content_stays_out_of_logs_and_audit`：讓它也走一次 HTTP 層（含 `?q=秘密` 的搜尋），斷言 span 與 JSON log 都不含秘密。

估計：後端 600–1,000 行（含測試），前端／E2E 100–200 行。

### 階段 1（M2 之後、正式多人使用前）

- 部署側：Prometheus＋Grafana（＋Alertmanager），幾條告警：5xx 比率、p95 延遲、worker `last_success` 停滯、`*_oldest_pending_age_seconds`、`pg_oldest_xid_holder_age_seconds`、池等待 p95、`released_in_transaction` 有增加。
- 前端自產 `traceparent`（I-9 自產版），後端在 span 記 trace-id；ErrorBox 顯示 request id 讓使用者回報時能附上。
- PG：`pg_stat_statements`、`log_min_duration_statement`（I-10），由部署方決定。

### 階段 2（等 Rust traces 穩定或確實需要時）

- `tracing-opentelemetry`＋`opentelemetry-otlp` → 自架 Jaeger v2（Badger）或 Tempo；取樣（例如錯誤與慢請求全留、其他 10%）。
- 有多台或要長期保存 log 時再加 Loki。
- 正式 SLO 與多窗口 burn-rate 告警。

---

## 5. 要使用者決定的 A 類問題

**A1. M2 是否允許為可觀測性新增依賴？**
- 選項：(a) 完全不加，連 `json` feature 都不開；(b) 只開 `tracing-subscriber` 的 `json` feature，其餘手寫（延續 `metrics.rs` 的作法）；(c) 加 `tower-http`、`prometheus-client` 等現成 crate。
- 建議：(b)。
- 代價：(b) 手寫 request-id middleware 與 RED 指標約 300 行，要自己測；(c) 少寫碼，但 `TraceLayer` 預設會記完整 URI，要自訂才安全，`prometheus-client` 要把現有指標搬過去。

**A2. `meta.request_id` 的語意要改成「整個請求同一個 id、也進 log」嗎？**
- 選項：(a) 改，並加 `X-Request-Id` 回應標頭；(b) 保持每次回應新產一個，另外加 `X-Request-Id`。
- 建議：(a)。現在的 id 和 log 對不上，沒有除錯價值。
- 代價：契約 02 §4.3 要改一句；前端不受影響（只顯示）。

**A3. 要不要接受客戶端送來的 id？**
- 選項：(a) 只自產；(b) 接受 `traceparent`，取 trace-id 當 correlation，request id 仍自產；(c) 也接受 `X-Request-Id`。
- 建議：(b)。`traceparent` 有固定格式可驗證；任意的 `X-Request-Id` 可被用來注入 log 內容。
- 代價：要寫格式驗證（無效就忽略）。

**A4. `/readyz` 要不要把 worker 停滯算成「not ready」？**
- 選項：(a) 只看 DB 與 migration；(b) 搜尋同步停滯也算 not ready；(c) 任何 worker 停滯都算。
- 建議：(a)。worker 停滯透過指標告警處理；readiness 失敗會讓反向代理把整個服務摘掉，連不受影響的頁面也打不開。
- 代價：worker 停滯時服務仍標示「ready」，要靠告警發現。

**A5. 部署時的檢視後端？**
- 選項：(a) Prometheus＋Grafana（AGPL）；(b) 只用 Prometheus 內建 UI＋Alertmanager，不用 Grafana；(c) SigNoz 一體化；(d) 先都不架，只保留 `/metrics` 和 JSON log 檔。
- 建議：M2 先 (d)，正式多人使用前上 (a)。
- 代價：(a) AGPL 要讓公司法務知道（內部自用不改原始碼通常沒有額外義務）；(c) 要維運 ClickHouse，對單一服務過重。

**A6. trace 後端（階段 2）選哪個？**
- 選項：(a) Jaeger v2（Apache-2.0、單一 binary、Badger）；(b) Grafana Tempo（AGPL，和 Grafana 整合）；(c) 不做 trace，只用 span＋log。
- 建議：先 (c)，真的需要時 (a)。
- 代價：(c) 看不到請求時間軸圖，要從 log 時間戳拼；(a) 多一個服務要維運。

**A7. E2E 時的後端 log 等級與保存？**
- 選項：(a) 維持 `warn`、不存檔；(b) `info,oxledger=debug`，存到 `web/test-results/`，失敗時附在報告上。
- 建議：(b)。
- 代價：log 檔變大（每次 E2E 幾 MB 等級，`test-results` 已在 E2E 流程裡清理）。

**A8. 連線池大小**（不是可觀測性本身，但指標上線後會直接看到）
- 選項：(a) 維持 sqlx 預設 10；(b) 改成可設定（`OXLEDGER_DB_MAX_CONNECTIONS`），預設 20 左右；(c) 背景 worker 用另一個小池，請求用主池。
- 建議：先 (b)，等池指標有數據再評估 (c)。
- 代價：(c) 要改 `App` 結構與所有 worker 的入口。

---

## 6. 研究過程

### 6.1 讀過的 repo 檔案

- `Cargo.toml`、`crates/oxledger/Cargo.toml`、`Cargo.lock`（確認沒有 `tower-http`、`opentelemetry*`、metrics crate；有 `log`、`tracing-log`）
- `crates/oxledger/src/metrics.rs`（全檔）、`src/main.rs`（全檔）、`src/db.rs`（`connect`、`pool_options`、`RELEASED_IN_TRANSACTION`）
- `src/api.rs`：路由組裝、`metrics_routes`、`api_guard`、`ok()`、`ApiFailure::into_response`
- `src/github/mod.rs::spawn_worker`、`src/ai/runner.rs::spawn_jobs`、`src/error.rs` 的 `request_id`
- `tests/s8_gate_extra.rs::hidden_content_stays_out_of_logs_and_audit`、`tests/p2_tx_cancel.rs`（只看計數器用法）
- `migrations/`（稽核與事件表、`github_event_jobs.attempts`、`ai_runs.lease_until`）
- `web/playwright.config.ts`、`web/e2e/backend.mjs`、`web/src/lib/api.ts`
- `docs/impl/p1-backend-gaps.md`（G20）、`docs/impl/s6b-checklist.md`（T-G35）、`docs/playbook.md`、`docs/design/06-m2-integration-contract.md`（`wiki_outbox_pending`）
- 本機 cargo registry 的 `sqlx-core-0.9.0/src/pool/{options.rs,inner.rs,mod.rs}`、`src/logger.rs`、`src/connection.rs`（唯讀，沒有下載新東西）

### 6.2 外部查詢與來源

| 主題 | 來源 |
|---|---|
| OTel Rust 狀態 | https://opentelemetry.io/docs/languages/rust/ ；https://github.com/open-telemetry/opentelemetry-rust （狀態表） |
| OTel Rust traces 穩定化時程（非正式） | WebSearch「opentelemetry-rust tracing API beta 1.0 stabilization traces 2026」所得的會議紀錄與追蹤文件摘要；Datadog 工程部落格 https://www.datadoghq.com/blog/engineering/rust-tracing-opentelemetry/ |
| tracing-opentelemetry | https://github.com/tokio-rs/tracing-opentelemetry |
| OTel JS／瀏覽器 | https://opentelemetry.io/docs/languages/js/ |
| W3C Trace Context | https://www.w3.org/TR/trace-context/ |
| RED | https://grafana.com/blog/2018/08/02/the-red-method-how-to-instrument-your-services/ |
| USE | https://www.brendangregg.com/usemethod.html （未在本次重新抓取，引用其公開定義） |
| SLO 告警 | https://sre.google/workbook/alerting-on-slos/ |
| sqlx 0.9 | https://docs.rs/sqlx/latest/sqlx/trait.ConnectOptions.html ；本機 sqlx-core 0.9.0 原始碼 |
| tower-http request-id | https://docs.rs/tower-http/0.6.8/tower_http/request_id （WebSearch 摘要） |
| Jaeger | https://www.jaegertracing.io/docs/latest/ |
| Grafana／Loki／Tempo 授權 | https://grafana.com/blog/2021/04/20/grafana-loki-tempo-relicensing-to-agplv3/ |
| SigNoz | https://www.ycombinator.com/companies/signoz ；https://techcrunch.com/2023/09/28/open-source-datadog-rival-signoz-lands-on-the-cloud-with-6-5m-investment |
| OpenObserve | https://www.cbinsights.com/company/openobserve/alternatives-competitors ；https://dealroom.co/companies/openobserve/ ；https://www.businesswire.com/news/home/20260715577163/en/ |
| SkyWalking | https://skywalking.apache.org/blog/2020-01-20-celebrate-200th-contributor/ ；https://www.tetrate.io/blog/what-is-apache-skywalking ；https://thenewstack.io/skywalking-apm-for-the-heterogeneous-new-stack/ |

### 6.3 踩到的坑

- **docs.rs 的 `sqlx::pool::Pool` 頁面 404**（`/latest/` 路徑在 0.9 有變動）。改讀本機 cargo registry 裡已存在的 sqlx-core 0.9.0 原始碼，比文件更準（例如 `acquire_slow_threshold` 預設 2 秒、`max_connections` 預設 10、慢語句預設 1 秒，文件頁都沒寫）。
- **「G35」對不上**：docs 裡沒有編號 G35 的連線池問題；只有 G20（p1-backend-gaps）和 GitHub 測試 T-G35。文件中標為待使用者確認，沒有自行推測內容。
- **OTel Rust 的穩定時程只找到非正式來源**（會議紀錄、社群追蹤文件），官方 README 只給狀態不給日期。導入前要再看當時的 release notes。
- **廠商來源偏多**：搜尋 OpenTelemetry Rust 時前幾筆是 OpenObserve 自己的教學頁；公司背景資料多來自 CB Insights、Dealroom 等商業資料庫，彼此的年份與金額不一致。表中只寫多數來源一致的部分。
- **SkyWalking 的貢獻者國別**：沒有找到 PMC 國別統計；排除依據是創辦人與起源（中國、華為支持），已在表中寫明依據。
- **Grafana Labs 總部**：本次搜尋結果沒有直接寫，表中標為「未直接查證」。
- **`tower-http` `TraceLayer` 的隱私陷阱**：預設 span 帶完整 URI，對 oxledger 的搜尋 API 會把搜尋字串寫進 log。這是選手寫 middleware 的主要理由之一。
- **sqlx 語句 log 帶 SQL 文字**：bind 參數不會進 log，但任何 `format!` 拼進 SQL 的內容會在 DEBUG 等級出現；目前 E4w-7 測試在 TRACE 等級跑仍是綠的（依測試設計推論，本次沒有實際執行測試）。

### 6.4 沒做的事

- 沒有執行任何測試或程式；所有現況描述來自讀原始碼。
- 沒有評估 VictoriaMetrics、Zipkin、Uptrace 等其他後端（時間與查證成本，若需要可另開一輪）。
