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

## 0019_m1_permissions、0020_estimated_hours_field_changes（M1.0）

- **不會讓 agent 工作脈絡失效**：兩個 migration 都沒有新增或修改 `role_permissions`、角色或成員，`security_epoch` 不變。
- 新權限名稱（`view_time_entries`、`log_time`、`edit_time_entries`、`edit_own_time_entries`、`log_time_for_other_users`、`triage_issues`、`manage_public_views`）只是允許出現；**沒有角色預設拿到**。之後給角色加這些權限時，和 0015 一樣會 bump 全部專案的 epoch，agent 要開新的工作脈絡。
- 新限制：只有 issue 類權限（`view_issues`、`add_issues`、`edit_issues`、`edit_own_issues`、`add_issue_notes`）可以限定 tracker。
  - 升級時若已有其他權限限定了 tracker，限制以 `NOT VALID` 留著（migration 會印 `NOTICE` 和筆數），既有列的行為不變；這些列之後不能再更新成同樣的形狀。
  - 查哪些列：`SELECT * FROM role_permissions WHERE NOT all_trackers AND permission NOT IN ('view_issues','add_issues','edit_issues','edit_own_issues','add_issue_notes')`。整理完後執行 `ALTER TABLE role_permissions VALIDATE CONSTRAINT role_permissions_tracker_scope`。
- `roles.time_entries_visibility`（預設 `all`）改變時也會 bump 全部專案 epoch（和 `issues_visibility` 一樣，值沒變不算）。
- `org_settings.week_start` 預設 1（週一）。
- `issues.estimated_hours`（`numeric(7,2)`）新欄位，既有 issue 都是空值。
- `issue_field_changes` 會從既有 `issue_events` 回填（已遮蔽的事件略過）；資料多時回填在 migration 交易內完成，期間會擋 `issue_events` 的寫入。

## 0021_views（M1.2，視圖與看板）

- **不會讓 agent 工作脈絡失效**：沒有新增或修改 `role_permissions`、角色或成員，`security_epoch` 不變。
- **0021 不給任何角色 `manage_public_views`**（建立與修改專案視圖要這個權限）。M1.4 的 migration 會照 Redmine 預設一次給內建角色 M1 新權限（Manager 拿到 `manage_public_views`），那時 bump 一次全部專案的 epoch，agent 要開新的工作脈絡。在 0021 與 M1.4 之間要用專案視圖，由管理員手動加（同樣會 bump epoch）。個人視圖不需要權限。
- **每個專案的分頁多兩個內建分頁**：「看板」（`board`，顯示）與「Triage」（`triage`，預設隱藏），接在既有清單最後。
  - 已經有 15 個分頁的專案升級後是 17 個；分頁數的 CHECK 從 5–15 改成 7–17。
  - 有改動的列 `lock_version` 加 1：升級前載入的分頁設定頁按儲存會得到 `conflict`（附最新設定），重新載入即可。
  - 預設分頁與 `is_default` 不變。
- 新表 `view_defs`、`view_def_events`、`view_def_event_redactions` 是空的；內建視圖不存在資料庫裡。

## 0022_triage（M1.3，Triage）

- **不會讓 agent 工作脈絡失效**：沒有新增或修改 `role_permissions`、角色或成員，`security_epoch` 不變。
- **0022 不給任何角色 `triage_issues`**（分流要這個權限）。M1.4 的 migration 會給 Manager（和其他 M1 新權限一起，那時 bump 一次 epoch）。在那之前要用 Triage，由管理員手動給角色（會 bump epoch）。
- **每個專案多一列 Triage 設定**（`project_triage_settings`），預設關閉、規則 `creator_without_triage`；之後新建的專案由 trigger 自動補。
- **既有的單不進收件匣**：`issue_triage` 是空的；只有啟用 Triage 之後新建的單才會進。
- **既有委派不會多出 `triage_decide`**：`proposal_ops` 與命令政策不變；委派頁的「新操作」會列出 `triage_decide`，委派人自己決定要不要加。新委派預設 direct。
- `issues` 多一個唯一限制 `issues_id_project_uq (id, project_id)`（契約 R25）；建索引時會短暫鎖 `issues`，大表升級請排在離峰。
- 四個 CHECK 用聯集重建：`delegation_command_policies_command_kind_check`、`delegations_proposal_ops_check`、`changeset_version_ops_kind_check` 加 `triage_decide`；`project_setting_events_kind_check` 加 `triage_settings_updated`。
