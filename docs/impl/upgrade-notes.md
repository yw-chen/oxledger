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
