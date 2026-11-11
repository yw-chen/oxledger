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

## 0023_time_tracking（M1.4，工時）

- **升級後要重建 agent 工作脈絡。**
  - 0023 照 Redmine 預設把 M1 新權限給內建角色（使用者 2026-11-10 決定）：Manager 拿全部 7 個（`view_time_entries`、`log_time`、`edit_time_entries`、`edit_own_time_entries`、`log_time_for_other_users`、`triage_issues`、`manage_public_views`），Developer 與 Reporter 拿 `view_time_entries`、`log_time`。
  - 授權寫成一個 `INSERT`，0010 的 epoch trigger 讓**每個專案的 `security_epoch` 恰好加一**。
  - 所以升級前建立的 agent 工作脈絡全部失效：agent 的請求回 `context_unavailable`（`details.kind = "quarantined"`）。
  - 要做的事：委派人在設定頁為每個 agent 開新的工作脈絡，重新發 token。舊的委派、政策與歷史都還在，不用重設。
- 角色用名稱找（`Manager`、`Developer`、`Reporter`，非 built-in）。改過名的角色拿不到，要由管理員手動加（加的時候同樣會讓 agent 工作脈絡失效）。
- **Non member、Anonymous 不給 `view_time_entries`**（和 Redmine 預設不同，待使用者決定）。公開專案的非成員看不到工時。
- **每個既有專案都啟用 `time_tracking` 模組**（這一步不另外 bump epoch）。不想用工時的專案，由專案管理員在工時設定關掉。
- 新增 5 個活動類別（開發〔預設〕、設計、測試、會議、其他）；每個專案一列 `project_time_settings`，之後新建的專案由 trigger 自動補。
- 組織工時規則預設：每日上限 0（不限）、可記 0 小時、可記未來日期、可記已關閉的單、不要求 issue、不要求說明。
- **既有委派不會多出工時操作**：`proposal_ops` 與命令政策不變，`read_own_time` 是 `false`。委派頁的「新操作」會列出 `log_time`、`update_time_entry`；新委派這兩個預設「需要核准」。
- 新增 SQL 函式 `time_role_visibility(project, viewer)`（只看角色）與 `time_visibility(project, viewer)`（工時模組開著才回前者），讀取與寫入都靠它判斷工時可見性。
- 四個 CHECK 用聯集重建：`delegation_command_policies_command_kind_check`、`delegations_proposal_ops_check`、`changeset_version_ops_kind_check` 加 `log_time`、`update_time_entry`（`delete_time_entry` 只給人，不在 agent 清單）；`agent_write_log_kind_check` 加 `time_write`。

## 0024_forms、0025_field_admin（M1.5，表單與欄位管理）

- **不會讓 agent 工作脈絡失效**：沒有新增或修改 `role_permissions`、角色或成員，`security_epoch` 不變（升級測試驗證）。不需要新權限：欄位管理限管理員，專案表單給管理員與有 `manage_project` 的人。
- 0024 新增 `form_defs`（每個 tracker 一張站台表單，另可有專案表單）、`form_rule_versions`、`form_layout_versions`、`form_rule_marks`；全部是空的，**升級後沒有任何 L1 規則**，寫入行為不變。`form_defs` 的變更會讓 `describe` 的 `schema_version` +1。
- `changeset_versions` 加 `rules_version`。**既有提案是 NULL**：在沒有 L1 規則之前不受影響；一旦有規則生效，這些提案在待審佇列標「規則已變」，只能逐份審查（不能勾選核准）。新提案建立時就寫入版本。
- 0025：`field_defs` 加 `deactivated_at`（停用）與 `lock_version`（既有欄位從 0 開始，PATCH 要帶）；新增 `custom_field_conversions`（Q-S9 型別轉換工作）。
- `org_settings` 加 `form_l0_version`（從 1 開始）：欄位的必填、停用、適用 tracker／專案、可見性與角色、刪除，以及選項的新增、刪除、停用會讓它 +1（trigger），規則版本字串（`rules_version` 的 `s` 段）跟著變；改名、排序、說明不會。agent 帶舊的 `expected_rules_version` 送出會收到 `form_rules_changed`，重新 `describe_form` 即可。
- 欄位的可見性與角色變更仍照 0010 的 trigger bump 相關專案 epoch（每個變更的部分各一次）。
- 型別轉換在背景分批跑；服務重啟時，欄位背景工作會接手 `running` 的轉換。轉完舊欄位不會自動停用，由管理員確認報告後自己停用。
- CHECK：都是新表的新 CHECK（`form_rule_versions`／`form_layout_versions` 的 `state`、`publish_mode`，`custom_field_conversions.state`），沒有重建既有 CHECK。
## 0026_user_preferences（M1.6，首頁與介面角色）

- **不會讓 agent 工作脈絡失效**：沒有新增或修改 `role_permissions`、角色或成員，`security_epoch` 與 `schema_version` 都不變（升級測試驗證）。不需要新權限：`/me/*` 只給登入的本人。
- 新增 `user_preferences`（每個使用者一列；`user_id` 必須是 user，trigger 擋 agent）。**升級後是空的**：使用者第一次 `GET /me/preferences` 或 `GET /me/home` 時以通用範本建立（需要你決定、指派給我、與我相關；左欄捷徑「指派給我」；所有專案預設列表），`onboarded` 是 `false`，所以每個既有使用者下次登入都會看到「你主要做什麼？」對話框（可略過）。
- 和契約 §2.8 的 DDL 差異：多一欄 `home_lock_version`（首頁有自己的樂觀鎖，改主題不會讓另一個分頁的首頁編輯衝突）；`home`、`nav_shortcuts`、`default_views` 加 `jsonb_typeof` CHECK。
- CHECK 是新建的（`interface_roles`、`theme`、`density`），沒有重建既有清單；測試比對 `pg_get_constraintdef` 與 `home::ROLES`／`THEMES`／`DENSITIES`。
- 偏好與首頁是個人狀態：不進 `issue_events`、不留稽核事件、不動任何 epoch。

## 0027_concepts（M1.7，搜尋）

- **不會讓 agent 工作脈絡失效**：沒有新增或修改 `role_permissions`、角色或成員，`security_epoch` 不變（升級測試驗證）。概念管理只給管理員（`is_admin`），沒有新權限；所有登入的人都能讀概念（非管理員只看到啟用的概念與啟用的標籤）。
- **升級後搜尋索引會自動重建一次（藍綠）。**
  - 索引 schema 從 2 升到 3（新增 `key_text`、`cf_text`、`notes` 三個文字欄位與 `status`、`priority` 兩個過濾欄位，`search_rev` 改 v2 涵蓋全部索引來源），查詢語意版本從 1 升到 2（BM25F 權重、別名展開）；analyzer 版本不變（1）。
  - 新版啟動時發現目前的 generation 是 schema 2／查詢語意 1：**照常用它服務，而且用舊語意查**（只查 subject ×3、description ×1，不查 key、筆記、自訂欄位，不做別名展開，`match.expanded` 一律沒有），同時在背景建新 generation；建好、追上 outbox、健康檢查通過後切換，舊 generation 依原規則保留 24 小時。切換前後搜尋都不中斷，也不會出現「舊 generation 用新語意查」的混搭。重建失敗時舊 generation 繼續服務，60 秒後再試；log 有 `search index upgrade failed`。
  - 重建期間：有 `q` 的搜尋結果和升級前一樣（筆記、自訂欄位、key 還搜不到，別名不展開）；條件搜尋不受影響。重建時間和 `oxledger reindex` 一樣，與 issue 數量成正比。
  - payload 是其他版本（比 schema 2 更舊、或比 3 更新，例如降版回舊程式）時，行為同 0018：從快照重建，完成前有 `q` 的搜尋回 `search_unavailable`。
- **搜尋現在也查**：issue key（整串比對，大小寫不分）、公開且未撤回、未遮蔽的筆記、可見（`visible = true`）且適用於該 issue 的 string／text／link 自訂欄位。私有筆記與受限欄位（`visible = false`）**一律不進索引**，有權限讀它們的人也搜不到（02 §7.5、Q-S2）。每個命中都在 SQL 用目前的資料重驗，所以筆記改私有、被遮蔽、欄位改受限或不再適用時，在索引追上之前就不會命中。
- 新增 outbox trigger：`issue_notes`、`issue_keys`、`issue_cf`（只有 string／text／link）、`field_defs`（`visible`、`is_for_all`、是否 dropping 有變時）、`field_trackers`／`field_projects`。欄位定義的變動會為每張有值的 issue 發一筆事件；欄位很多值時這是一次較大的寫入（管理員操作）。同一個交易裡同一張 issue 只發一筆（`outbox_issue_once`；0001 的 `issues_enqueue` 也改成這個規則），所以「改欄位＋加筆記」的命令仍是一筆訊息。
- **既有概念標籤（R18）**：0027 之前用 seed 或匯入寫進 `aliases`（`entity_kind = 'concept'`）的標籤，每個概念 id 會補一列 `concepts`（偏好標籤取 `pref` 標籤，沒有就取 bytewise 最小的標籤；建立者是 system principal）。標籤用 `casefold(normalize(label, NFC))` 比較（大小寫、NFC／NFD 視為相同；`casefold` 跟資料庫的 collation，和其他名稱規則一樣），**撞名的不會讓 migration 失敗**：每組只留一個 `active`（`pref` 標籤優先，再來是最舊的概念、bytewise 最小的標籤），其他改成 `staging`（不展開），並以 `NOTICE` 印出筆數。接著每個概念保證只有一個啟用的 `pref` 標籤（就是概念名稱）：被降級的 `pref` 改成 `alt`；沒有啟用 `pref` 的概念從剩下的啟用標籤挑一個當名稱（`alt` 優先於 `hidden`，再取 bytewise 最小）；一個啟用標籤都不剩的概念改成 `retired`（標籤保留為 `staging`）。升級後請管理員到概念管理檢查 `staging` 標籤與被退役的概念，可以做的處理是：保留 `staging`（不展開、不佔名稱，非管理員看不到）、改成 `hidden`（`PATCH …/labels/{label} {kind: hidden}`）、把概念改名（`PATCH /admin/concepts/{id} {pref_label}`）或重新啟用。目前**沒有刪除標籤或概念的 API**（列為後續項）。
- 新的部分唯一索引 `aliases_concept_label_uq`（只限 `entity_kind = 'concept' AND status = 'active'`）建立在正規化後的字串上；PostgreSQL 大版本升級若改了 Unicode 版本，依 PostgreSQL 的說明對這個索引 `REINDEX`。
- 概念與標籤的變更不寫 `issue_events`，也不動任何 epoch；改了概念之後，下一次搜尋就用新的展開（展開在查詢時做，不需要重建索引）。

## 0028_notifications（M1.8，通知）

- **不會讓 agent 工作脈絡失效**：沒有新增或修改 `role_permissions`、角色或成員，`security_epoch` 不變（升級測試驗證）。關注、通知、偏好都是個人狀態，不寫 issue 歷史、不動 `lock_version`。
- **升級之前的事件不會補送**：0028 新增 outbox consumer `notify`（cursor 從升級當下開始），並為每張既有 issue 在 `notification_issue_progress` 回填目前最後一個事件的序號；升級後第一個新事件只送那一個。升級後沒有任何關注（關注表是新的），通知從「建立者、指派對象、之後的關注」開始累積。
- **outbox GC 會等 `notify`**：`notify` 是 active 的 cursor，派送落後時 outbox 不會被清。`/metrics` 新增 `notify_outbox_pending`、`notify_outbox_oldest_pending_age_seconds`、`notify_dispatch_delay_seconds`、`notify_rounds_total`、`notify_events_total`、`notify_deliveries_total`、`notify_last_success_timestamp_seconds`；`notify_outbox_pending` 持續上升代表派送卡住。目前沒有停用通知的開關（刪掉 `notify` cursor 列，下一輪會從頭補建，progress 會擋住重送）。
- `serve` 每秒跑一輪派送，每小時清一次過期通知（已讀 30 天、未讀 90 天）。一輪內平行派送的 worker 數由環境變數 `OXLEDGER_NOTIFY_WORKERS` 決定（1–16，預設 4；超出範圍用 4 並記 warn）。**每個 worker 派送時佔一條資料庫連線**，和 API 請求共用同一個連線池（sqlx 預設 10 條）：worker 數要留給 API 足夠的連線，例如連線池 10 條時不要設超過 4–5。
- 同一張 issue 連續 5 次派送失敗（不含連線類錯誤）時，它當時待送的事件會被跳過（log 有 `skipping the issue's pending events`，`/metrics` 的 `notify_skipped_issues_total` 增加），其他 issue 不受影響。
- 新增 `user_column_guard()`：關注、通知、偏好、靜音只能屬於 user（agent、群組寫入回 `check_violation`）。
- 新 API（前端要接）：`GET /notifications`、`GET /notifications/unread-count`、`POST /notifications/{id}/read`、`POST /notifications/{id}/unread`、`POST /notifications/read-all`、`PUT|DELETE /issues/{key}/watch`、`GET|PUT /me/notification-preferences`；`GET /issues/{key}` 多了 `watch`（只有人登入時有）。沒有 email。

## 0029_ai_team（M1.9，AI 團隊）

- **不會讓 agent 工作脈絡失效**：沒有新增或修改 `role_permissions`、角色或成員，`security_epoch` 不變（升級測試驗證）。
- **既有 agent 自動搬進 runtime**：每個既有的 agent principal 各得到一個外部 runtime（`backfilled = true`、`isolation_ack = true`），角色是 `personal`（範本 `personal` v1）。runtime 名稱取 client 名稱前 56 字，同一個人撞名時加 `-2`、`-3`…（最長 60 字）；`created_by_session` 取該 principal 最新的脈絡（不論狀態），沒有脈絡時是空的。因為每個 principal 原本就有自己的脈絡，搬完後每個 runtime 的 R 和原來相同，不會變寬或變窄。
- **既有的脈絡、委派、token 都綁到該 runtime**：`agent_work_contexts.agent_principal_id` 改名為 `created_for_principal_id`（只供稽核）；委派與 token 補上 `runtime_id`，token 補上 `agent_principal_id`。**既有 token 升級後照常可用**，身分與可讀範圍不變；既有委派不受影響（升級測試驗證）；未處理的提案只多一欄 `chain_review`（NULL），升級測試沒有帶提案。
- **之後的行為改變**：
  - 同一個人、同一個 client 只能有一個未退役的 runtime（`runtime_client_taken`，409）。要重來請在同一個 runtime 開新脈絡，或先退役它。
  - 同一個 runtime 的所有角色共用一個脈絡與 R：隔離（epoch 變動）、斷路器暫停、開新脈絡都會讓整個 runtime 的角色一起停；錯誤的 details 有 `shared_with`。
  - 新的鏈上限：同一張單在沒有人介入下被 agent 連續寫入超過 20 次，或換手深度超過 2，原本會直接寫入的命令改為送審（`review_required`，reason `agent_chain`）。人的寫入、undo、核准套用都會重新計算。
  - 暫停：贊助人或站台管理員可暫停整個角色，專案管理者可暫停角色在自己專案的寫入；暫停中所有寫入（含核准後套用）回 `forbidden_op`（reason `agent_paused`）。恢復時會重查斷路器（脈絡停用中回 `agent_suspended`）。
  - 站台管理員暫停會記在 runtime（新欄位 `agent_runtimes.admin_paused_at`／`admin_paused_by`／`admin_paused_reason`）。同一個人之後在這個 runtime 或同一個 client 新建的角色，一建立就是暫停（trigger `agent_principals_inherit_admin_pause`），退役再重開繞不過，只有站台管理員能解除。升級時這些欄位都是空的。
  - 暫停資訊：`pause.scope` 固定是 `principal`；新增 `pause.paused_by`（`sponsor`／`site_admin`）與專案政策列的 `paused_by`（`sponsor`／`site_admin`／`project`），只給看得到完整資訊的人。
  - AI 團隊頁的可見性：站台管理員看得到所有外部角色（含沒有委派的）；其他人要在該角色被委派的專案是成員（本人或所屬群組）才看得到，公開專案的非成員看不到；內建 AI 不出現在列表與詳細頁（M1.10 再定）。
  - 活動頁與統計逐筆過濾：只寫了觀看者讀不到的私人備註或受限欄位的寫入不會出現，也不計數。02 §16.10 的贊助人活動頁一起變嚴。
  - `POST /delegations`（舊路徑）新建角色時也受每人 20 個角色上限（`invalid_value`，kind `role_limit`）。
  - 錯誤細節：發 token 時有多個角色回 `invalid_argument`，`details.reason = "principal_required"`（不是 `details.kind`）。
  - `describe agent_context`：只列這個角色自己的委派專案；同 runtime 其他角色帶進 R 的專案只給數量 `other_role_projects`（0 時不出現）。
- **02 留下的 `oxledger-ai@…` 外觀 handle**：02 允許使用者把 agent 的 handle 取成 `oxledger-ai@…`，看起來像內建 AI。0029 **不改這些資料**，只在升級時用 `RAISE NOTICE` 印出數量（`0029: N agent handle(s) start with oxledger-ai@ and look like the built-in AI; see upgrade-notes.md`）。0029 之後新建的外部角色已經不能用這種 handle。管理員可以用下面的查詢找出來，自行決定要不要請使用者改名：

  ```sql
  SELECT ap.agent_principal_id, ap.handle
  FROM agent_principals ap
  JOIN agent_runtimes r ON r.id = ap.runtime_id
  WHERE r.kind = 'external' AND ap.handle LIKE 'oxledger-ai@%';
  ```

  升級時要不要自動改名，**待使用者決定**。
- `agent_write_log.kind` 的 CHECK 重建為既有值的聯集再加 `chain_review`、`time_taint_review`（後者給 M1.10）。
- 新 API（前端要接）：`GET|POST /ai-team`、`GET /ai-team/templates`、`GET /ai-team/{id}`、`GET /ai-team/{id}/activity`、`POST /ai-team/{id}/pause|resume|retire|credentials`、`GET /me/agent-runtimes`、`PATCH /me/agent-runtimes/{id}`、`POST /me/agent-runtimes/{id}/new-context|retire`。`POST /delegations` 新增 `agent_principal_id`（和 `agent_client_id` 擇一）；`/agent-contexts*` 保留為相容別名。

## 0030_ai_chat（M1.10，右欄 AI 對話）

- **不 bump `security_epoch`**：不動角色、權限、成員，不新增 principal（升級測試驗證）。既有 agent 脈絡與 token 不受影響。
- **預設關閉**：`ai_settings` 一列，`enabled = false`、沒有 provider、沒有月預算；內建 client `oxledger-ai` 建好但停用。管理員在「AI 設定」頁加 provider、按「測試連線」、填月預算後才能啟用。
- **升級前要處理**：如果 02 時代有人建了**一般** agent client 叫 `oxledger-ai`，0030 會中止（`agent_clients already has a client named oxledger-ai; rename it before upgrading`）。先改名再升級：
  ```sql
  UPDATE agent_clients SET name = 'oxledger-ai (02)' WHERE name = 'oxledger-ai' AND NOT builtin;
  ```
  中止時 0030 整個沒有套用（交易回滾），改名後重跑即可。
- `changesets` 0009 的 `CHECK (origin = 'external_agent')` 換成具名的 `changesets_origin_agent_ck`（`external_agent`、`builtin_ai`）與 `changesets_builtin_ctx_ck`。
- **建置**：要連 provider 的建置加 feature `ai`（`cargo build --features ai`；用既有的 hyper、hyper-rustls、rustls ring、webpki-roots，沒有新 crate）。沒有 `ai` 的建置照常啟動，AI 頁面與 API 都在，但 run 會立刻以 `ai_unavailable`（`not_configured`）結束，`/admin/ai/endpoints` 的 `transport` 是 `false`。
- **部署設定（環境變數，重啟後生效）**：
  - `OXLEDGER_AI_ENDPOINT_<NAME>=https://host[:port][/path]`：登記一個端點。`<NAME>` 要符合 `^[A-Z][A-Z0-9_]{0,39}$`。URL 不能有帳密（`user:pass@`）、query、fragment；只收 http／https。Anthropic 端點填到主機（程式加 `/v1/messages`）；OpenAI 相容端點填到 `/v1`（程式加 `/chat/completions`）。
  - `OXLEDGER_AI_KEY_<NAME>`：該端點的 key（Anthropic 必填；OpenAI 相容可省略，省略就不送 `Authorization`）。key 只在記憶體，不進 DB、日誌、錯誤、回應。
  - `OXLEDGER_AI_CA_FILE_<NAME>`：PEM 檔，內部 CA 憑證（可多張），和內建的 webpki-roots 一起信任。讀不到或格式錯，該端點不登記。
  - `OXLEDGER_AI_ALLOW_HTTP=1`：允許 `http://` 連到 loopback 與私有位址（例如本機 Ollama）。**http 到公網一律拒絕**（開關打開也一樣）；https 到私有與 loopback 位址不需要這個開關。link-local、雲端 metadata（169.254.169.254 等）、0.0.0.0、multicast、site-local `fec0::/10`、Teredo 一律拒絕，開關打開也一樣。IPv4-mapped IPv6 照 IPv4 判斷；NAT64（`64:ff9b::/96`）、IPv4-compatible（`::/96`）、IPv4-translated（`::ffff:0:0:0/96`）、6to4（`2002::/16`）照裡面的 IPv4 判斷，而且只有公網位址能過（本機或私有的 provider 請直接登記它的 IPv4／IPv6 位址或主機名）。
  - `OXLEDGER_AI_D17_EXTRA_HOSTS`、`OXLEDGER_AI_D17_EXTRA_MODELS`：逗號分隔，追加到內建的 D17 主機與模型名清單（大小寫不分）。命中主機清單的端點啟動時不登記；命中模型名的 provider 存不進去（`ai_model_disallowed`）。
  - 啟動時 log 會列出登記的端點名稱與沒登記的原因（不含 URL 與 key）。
- **每次連線**都重新解析主機、檢查所有位址、釘在驗過的 IP 連線（防 DNS rebinding）；不跟隨重導向；回應上限 8 MiB。
- 背景工作：每 15 秒清租約過期的 run（租約 30 秒、每 10 秒續期；程序死掉的 run 最慢 45 秒內結束成 `failed`／`interrupted` 並釋放預算保留），每小時清保存期限到期的對話內容（預設 30 天；清除後 90 天刪列，`ai_runs` 保留成本與計數）。
- **改使用中 provider 的端點或模型**會把 AI 關掉（`ai_settings.enabled = false`，事件 `settings_updated`，`reason: provider_config_changed`），要重新「測試連線」再啟用；既有對話下一則要確認 `ai_provider_changed`（`ai_conversations.last_provider_config_at` 記住對話上次用的設定版本）。
- 每次模型請求的輸入估算（ASCII 字元 ¼ token、其他字元 1 token）上限 16 000：超過時從最舊的頁面脈絡與工具結果開始截短；仍放不下就不送，run 以 `budget_exhausted`（`run.finished` 的 `error.kind = request_input_limit`）結束。
- 結算：provider 回報的用量照實記錄，即使超過保留額（超過時月預算照實累計，可能觸發 100% 通知）；只有估算的用量（沒回 usage）以保留額為上限。
- 新 API（前端要接）：
  - 使用者：`GET /ai/status`、`POST /ai/assistant`、`POST /ai/assistant/extend`、`GET|POST /ai/conversations`、`DELETE /ai/conversations/{id}`、`GET|POST /ai/conversations/{id}/messages`（POST 回 202 與 `stream_url`，要 `Idempotency-Key`）、`GET /ai/runs/{id}`、`GET /ai/runs/{id}/stream`（SSE，支援 `Last-Event-ID`）、`POST /ai/runs/{id}/cancel`。
  - 管理員：`GET|PUT /admin/ai/settings`、`GET /admin/ai/endpoints`、`GET|POST /admin/ai/providers`、`PATCH /admin/ai/providers/{id}`、`POST /admin/ai/providers/{id}/test`、`POST /admin/ai/providers/{id}/retire`、`GET /admin/ai/usage`、`GET /admin/ai/events`。
  - `POST /me/agent-runtimes/{id}/new-context` 的 `carry_over_delegations: true` 現在可用（M1.9 是拒絕）。
  - 新通知種類 `ai_budget`（月預算 80%、100%，給啟用中的站台管理員）。

