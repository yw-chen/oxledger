# 本機 Docker 部署

在一台機器上用 Docker 跑 oxledger，只給這台機器自己用：瀏覽器開 `http://localhost:8088`。

三個容器：

| 服務 | 映像檔 | 做什麼 |
|---|---|---|
| `db` | `postgres:18` | 資料庫；不對外開 port |
| `app` | `oxledger-server:local` | `oxledger serve`；啟動時自動跑 migration |
| `web` | `oxledger-web:local` | Caddy：送前端靜態檔，把 `/api`、`/mcp`、`/healthz`、`/readyz` 轉給 `app` |

前端和 API 必須在同一個網址底下：伺服器用 `OXLEDGER_SPA_ORIGIN` 檢查每個非 GET 請求的 `Origin`，登入 cookie 是 `__Host-`（Secure），瀏覽器只在 https 或 `localhost` 才收。所以**只能用 `http://localhost:<port>` 開**，`127.0.0.1` 或區網 IP 都不行。要給別台電腦用，得另外加 https。

## 第一次啟動

以下指令都在 repo 根目錄執行。

1. 建 `deploy/.env`（不進 git），把密碼改掉：

   ```powershell
   Copy-Item deploy\.env.example deploy\.env
   notepad deploy\.env
   ```

2. 建置並啟動（第一次建置 Rust 要十幾分鐘；Dockerfile 預設 6 個編譯 jobs）：

   ```powershell
   docker compose -f deploy/compose.yaml up -d --build
   ```

   確認起來了：`curl.exe http://localhost:8088/readyz` 回 `"status":"ready"`。

3. 載入預設設定（角色、tracker、狀態、workflow；只能在全新的站跑一次）：

   ```powershell
   docker compose -f deploy/compose.yaml exec app oxledger setup init-defaults
   ```

4. 建系統管理員（會問密碼，至少 12 個字元，不會顯示在畫面上）：

   ```powershell
   .\deploy\add-user.ps1 -Login admin -Name "管理員" -Admin
   ```

5. 建專案、把人加進去：

   ```powershell
   docker compose -f deploy/compose.yaml exec app oxledger project add WEB "官方網站"
   .\deploy\add-user.ps1 -Login alice -Name "Alice"
   docker compose -f deploy/compose.yaml exec app oxledger member add WEB alice 開發者
   ```

6. 瀏覽器開 `http://localhost:8088` 登入。

## 預設設定的內容

`setup init-defaults` 參考 Redmine 的「載入預設設定」：

- **狀態**：新建、進行中、回饋（進行中類）、已解決、已關閉、已拒絕（完成類）。
- **tracker**：缺陷、功能、支援，新單都從「新建」開始。
- **角色**：

  | 角色 | 看議題的範圍 | 能做的事 |
  |---|---|---|
  | 管理者 | 全部 | 所有權限：議題、版本、專案設定、分流、工時（含替別人記）、wiki 管理、匯入 |
  | 開發者 | 預設 | 開單、編輯、留言、私密留言與私密議題、關聯、記自己的工時、讀寫 wiki |
  | 回報者 | 預設 | 開單、留言、記工時、讀 wiki |
  | 非成員（內建） | 預設 | 公開專案：看議題、開單、留言、讀 wiki |
  | 匿名（內建） | 預設 | 公開專案：看議題、讀 wiki |

  沒有「只看自己的單」的角色，所以預設角色的專案都可以委派給 agent。
- **workflow**：管理者可以在任兩個狀態之間轉換；開發者在新建、進行中、回饋、已解決之間轉換（不能關閉或拒絕）；回報者沒有編輯權限，不能改狀態（只能開單、留言）。

目前還沒有管理畫面，要改角色、權限、workflow 只能直接改資料庫。

## 指令一覽

都用 `docker compose -f deploy/compose.yaml exec app oxledger …` 執行。

| 指令 | 說明 |
|---|---|
| `setup init-defaults` | 載入預設設定；站上已有任何角色、tracker 或狀態時拒絕 |
| `project add <KEY> <名稱> [--public] [--modules issue_tracking,time_tracking,wiki]` | 建專案；KEY 是議題編號前綴（2–10 個大寫英數字，字母開頭）；預設開三個模組、不公開 |
| `project list` | 列出專案 |
| `role list` | 列出角色 |
| `member add <KEY> <帳號> <角色>[,<角色>…]` | 把人加進專案；已經是成員就補上角色；多個角色用逗號或空白分開 |
| `user add <帳號> <名稱> [--admin] [--email <email>] [--no-password]` | 建帳號，密碼從 stdin 讀（用 `add-user.ps1`）；`--email` 是這個人登入用的 Google 帳號；`--no-password`（要搭配 `--email`）不讀密碼，這個人只能用 Google 登入 |
| `user set-email <帳號> <email>`／`--clear` | 改或清掉登入用的 Google email；這個人的 session 與原本連結的 Google 帳號都會失效 |
| `user set-password <帳號>` | 改密碼並登出所有 session，密碼從 stdin 讀 |
| `user lock` / `user unlock <帳號>` | 鎖住、解鎖帳號 |
| `reindex` | 請執行中的伺服器重建搜尋索引（議題與 wiki） |

在 PowerShell 用管線把密碼傳給 `docker compose exec` 時，Windows PowerShell 會在前面加 BOM；伺服器會去掉它，但仍建議用 `add-user.ps1`。要改密碼可以在 Git Bash 執行：

```bash
printf '%s' '新密碼' | docker compose -f deploy/compose.yaml exec -T app oxledger user set-password <帳號>
```

## 日常操作

```powershell
docker compose -f deploy/compose.yaml ps          # 狀態
docker compose -f deploy/compose.yaml logs -f app # 伺服器記錄
docker compose -f deploy/compose.yaml stop        # 停止（資料保留）
docker compose -f deploy/compose.yaml start       # 再啟動
```

`restart: unless-stopped`：Docker Desktop 開著時，重開機後會自動啟動。

### 備份與還原

資料在三個 volume：`oxledger_db`（資料庫）、`oxledger_files`（附件檔案）、`oxledger_data`（搜尋索引，可以從資料庫重建）。附件要和資料庫在同一個時間點備份（先 `stop app` 再備份兩者最保險）；下面只示範資料庫，附件用 `docker compose -f deploy/compose.yaml cp app:/files .\oxledger-files` 複製出來。dump 先寫在容器裡再複製出來——**不要用 `>` 把 `pg_dump` 的輸出導到檔案**，PowerShell 會把二進位內容當文字轉碼，備份檔會壞掉：

```powershell
docker compose -f deploy/compose.yaml exec -T db pg_dump -U oxledger -Fc -f /tmp/oxledger.dump oxledger
docker compose -f deploy/compose.yaml cp db:/tmp/oxledger.dump .\oxledger.dump
docker compose -f deploy/compose.yaml exec -T db rm /tmp/oxledger.dump
```

還原到全新的部署（只先啟動 `db`，在 `app` 第一次啟動之前還原；搜尋索引在 `app` 啟動後自動重建）：

```powershell
docker compose -f deploy/compose.yaml up -d db
docker compose -f deploy/compose.yaml cp .\oxledger.dump db:/tmp/oxledger.dump
docker compose -f deploy/compose.yaml exec -T db pg_restore -U oxledger -d oxledger --no-owner /tmp/oxledger.dump
docker compose -f deploy/compose.yaml exec -T db rm /tmp/oxledger.dump
docker compose -f deploy/compose.yaml up -d
```

### 升級

拉新版程式碼後重建並重啟；migration 在 `app` 啟動時自動跑：

```powershell
docker compose -f deploy/compose.yaml up -d --build
```

### 全部刪掉重來

```powershell
docker compose -f deploy/compose.yaml down -v   # -v 會刪掉資料庫與索引，無法復原
```

## Google 登入（選用）

設定之後，登入頁多一個「使用 Google 登入」；密碼登入照舊。規則：

- **不開放自行註冊**。Google 帳號只能登入「管理員先建好、email 相同」的帳號；沒有對應帳號的人會被擋下。第一次登入時把那個 Google 帳號和帳號連結起來，之後認 Google 帳號本身，對方改 email 也不受影響。
- **全新的站可以用 Google 開站**。站上一個帳號都沒有時，第一個用 Google 登入的人會看到「建立機構」表單（機構名稱、時區、自己的帳號與姓名、要不要載入預設設定），送出後成為第一位系統管理員。這取代「第一次啟動」的步驟 3、4。

步驟：

1. 在 [Google Cloud Console](https://console.cloud.google.com/apis/credentials) 建一個 OAuth 用戶端（類型：網頁應用程式），「已授權的重新導向 URI」填 `http://localhost:8088/api/v1/auth/google/callback`（port 跟 `OX_PORT` 一樣）。
2. 把用戶端 ID 與密鑰寫進 `deploy/.env`：

   ```
   OX_GOOGLE_CLIENT_ID=….apps.googleusercontent.com
   OX_GOOGLE_CLIENT_SECRET=…
   OX_SETUP_EMAILS=you@example.com
   ```

   `OX_SETUP_EMAILS`（選用，建議設）限制誰可以開站；不設的話，站上沒有帳號的期間，第一個登入的 Google 帳號就會成為管理員。`OX_GOOGLE_ALLOWED_DOMAINS`（選用）限制只有這些網域的 email 可以登入。
3. 重啟：`docker compose -f deploy/compose.yaml up -d`。伺服器記錄會有 `Google sign-in on`。
4. 之後幫同事建帳號時帶上 email（不需要密碼）：

   ```powershell
   docker compose -f deploy/compose.yaml exec app oxledger user add alice "Alice" --email alice@example.com --no-password
   ```

   既有帳號用 `user set-email` 補上 email，就可以改用 Google 登入，密碼仍然有效。

需要再確認身分的動作（委派 agent、發 token）：有密碼的人輸入密碼；只用 Google 登入的人會被帶去 Google 再回來。Google 不保證會要求重新輸入密碼，所以這一步只確認「瀏覽器裡登入的還是同一個 Google 帳號」。

## 選用設定

內建 AI、GitHub、`/metrics` 的環境變數（`OXLEDGER_AI_*`、`OXLEDGER_GITHUB_*`、`OXLEDGER_METRICS_TOKEN`，見 `crates/oxledger/src/main.rs` 開頭與 `docs/playbook.md`）要寫進 `deploy/.env`，並加到 `compose.yaml` 的 `app.environment`。GitHub 私鑰檔要另外掛進容器。

## 已知限制

- 只給本機用：沒有 https，只能從 `localhost` 開。
- 沒有管理畫面：專案、成員、角色只能用上面的指令或直接改資料庫；還不能刪專案、移除成員、建群組。
- 附件單檔上限 20 MB、每人每天 1 GB；`/files` 所在磁碟剩不到 5 GB 時拒絕上傳（`OXLEDGER_ATTACHMENT_MAX_BYTES`、`OXLEDGER_UPLOAD_DAILY_BYTES`、`OXLEDGER_FILES_MIN_FREE_BYTES` 可調）。
- Git commit 連結需要 GitHub App 與 webhook，本機部署沒有設定。
- 停止容器時伺服器不做 graceful shutdown，正在處理的請求會中斷。
- 伺服器看到的來源 IP 一律是 Caddy 容器，所以登入失敗次數的每 IP 上限是全站共用的。
- Caddy 只轉 `/api`、`/mcp`、`/healthz`、`/readyz`；GitHub webhook 與 `/metrics` 沒有轉（本機也收不到 GitHub 的 webhook）。
- 「已解決」歸在完成類：篩選「未完成」、版本進度都把它當成已完成（和 Redmine 的 Resolved 不同）。
