# 升級說明

每一條寫：哪個 migration、升級後會發生什麼、要做什麼。

## 0015_project_tabs（S6b.6，專案分頁）

- **升級後要重建 agent 工作脈絡。**
  - 這個 migration 新增專案權限 `manage_project`，並給 Manager 角色。
  - 新增 `role_permissions` 列會觸發 0010 的 epoch trigger，所有專案的 `security_epoch` 都加一。
  - 所以升級前建立的 agent 工作脈絡全部失效：agent 的請求會回 `context_unavailable`（`details.kind = "quarantined"`）。
  - 要做的事：委派人在設定頁為每個 agent 開新的工作脈絡，重新發 token。舊的委派、政策與歷史都還在，不用重設。
  - 這是使用者 2026-11-08 對 B17 的決定（接受 epoch bump、寫進升級說明），沒有改 trigger。
- 每個專案自動建出分頁設定列（五個內建分頁，預設「時程」）。之後新建的專案由 trigger 在同一個交易建列。
- 不是 Manager、但需要管理分頁或 GitHub 連動的角色，要由管理員手動加 `manage_project`。加的時候同樣會讓 agent 工作脈絡失效。

## 0018_search_generations（P2，搜尋收尾）

- **第一次啟動會從快照重建搜尋索引。**
  - 索引目錄改成 `<OXLEDGER_INDEX_DIR>/CURRENT` 加 `gen-<uuid>/`。舊版直接放在根目錄的索引沒有 `CURRENT`，會被略過（檔案不刪，確認新版正常後可以手動刪掉根目錄下的舊檔：`meta.json`、`*.idx` 等，不要刪 `gen-*` 與 `CURRENT`）。
  - 重建在背景跑，伺服器照常啟動；完成前有 `q` 的搜尋回 `search_unavailable`（可重試），純條件搜尋不受影響。
  - migration 刪掉舊的 `outbox_cursors` 列 `consumer = 'search'`；新的 consumer 是 `search@<generation>`。
- **重建改成藍綠**：`oxledger reindex` 只是請正在跑的伺服器在背景重建（搜尋不中斷）；伺服器沒在跑時用 `oxledger reindex --offline`。舊 generation 保留 24 小時後由每小時的清理工作刪除。
- **outbox 開始清理**：每小時刪掉所有搜尋 generation 都已套用、而且超過 1 小時的列。
- **`/metrics`**：設定 `OXLEDGER_METRICS_TOKEN` 才開，要帶 `Authorization: Bearer <token>`。監控帳號要有 `pg_read_all_stats`，`pg_oldest_xid_holder_age_seconds` 才看得到其他使用者的交易。
- `issues (updated_at DESC, id DESC)` 新索引用一般的 `CREATE INDEX` 建立，大量資料時建立期間會擋寫入；正式環境資料多時，先手動 `CREATE INDEX CONCURRENTLY issues_updated_idx ON issues (updated_at DESC, id DESC)` 再升級；migration 用 `IF NOT EXISTS`，已經存在就略過。
- 自訂欄位多了 `definition_version`（從 1 起）與 `unit`；改型別、改單位、選項新增／改名／停用／刪除都會讓版本 +1。
