# M1.10 手動檢查（真的 provider）

自動測試只對測試內的假 provider 跑（`m1_10_ai_run.rs`）。上線前要用真的 Anthropic key 與一個自架 OpenAI 相容端點各跑一次，結果記在本檔最後。**這些檢查還沒有人跑過。**

用公司帳號的 key；不要用個人帳號。

## 準備

1. 用 feature `ai` 建置：`cargo build -p oxledger --release --features ai`。
2. 設環境變數後啟動（例）：
   ```
   OXLEDGER_AI_ENDPOINT_ANTHROPIC=https://api.anthropic.com
   OXLEDGER_AI_KEY_ANTHROPIC=<公司帳號的 key>
   OXLEDGER_AI_ENDPOINT_LOCAL=http://<內網主機>:8000/v1
   OXLEDGER_AI_ALLOW_HTTP=1          # 只有內網端點走 http 時需要
   OXLEDGER_AI_CA_FILE_LOCAL=<內部 CA 的 PEM>   # 內網端點走 https 且用內部 CA 時
   ```
3. 啟動 log 應列出 `ANTHROPIC`、`LOCAL` 已登記，沒有 URL 與 key。

## Anthropic

1. 管理員：AI 設定 → 新增 provider（Anthropic、端點 `ANTHROPIC`、模型照預設），勾 D17 與「公司帳號」確認 → 測試連線 → 應顯示成功、`supports_tools = true`。
2. 填月預算、啟用。
3. 一般使用者：右欄 → 設定內建 AI → 委派一個測試專案（`add_note` 設 direct）。
4. 問：「讀 <某張單的 key>，摘要後在上面留一則筆記『AI 測試』」。
   - 右欄看到逐字出現的回覆、「讀取 …」「寫入」進度、寫入結果帶「復原」連結。
   - 那張單有筆記，作者顯示內建 AI。
5. `SELECT input_tokens, output_tokens, cost_usd, usage_estimated, stop_reason FROM ai_runs ORDER BY started_at DESC LIMIT 1;` → usage 不是 0、`usage_estimated = false`、`end_turn`。和 Anthropic console 的用量大致相符。
6. 執行中按「停止」→ run `canceled`。
7. 把 key 改成錯的重啟 → 送出 → 回覆顯示 provider 錯誤（`http_status` 401），畫面、log、`ai_runs`、`ai_provider_events` 都沒有 key。
8. 在 log 檔裡搜尋 key 的前 8 個字元，應找不到。

## 自架 OpenAI 相容（vLLM 或 Ollama）

1. 新增 provider（OpenAI 相容、端點 `LOCAL`、填模型名）→ 測試連線。記下 `supports_tools`。
2. 切成這個 provider 再送舊對話 → 應先跳「AI 模型已更換」確認，確認後繼續。
3. 重複 Anthropic 的第 4、5 步。`usage_estimated` 若是 `true`，表示伺服器不回 `include_usage`（記下來）。
4. `supports_tools = false` 時：對話只用頁面脈絡回答，不會有寫入。

## 結果

| 日期 | 誰 | provider | 結果 | 備註 |
| --- | --- | --- | --- | --- |
| | | | | |
