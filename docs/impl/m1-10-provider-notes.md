# M1.10 provider 文件對照

讀取日期：2026-11-11。只讀了公開文件，沒有對真的 provider 送請求（見 `m1-10-manual-check.md`）。程式裡的對應在 `crates/oxledger/src/ai/protocol.rs`、`client.rs`。

## Anthropic Messages API

來源：

- https://platform.claude.com/docs/en/api/messages
- https://platform.claude.com/docs/en/build-with-claude/streaming
- https://platform.claude.com/docs/en/api/versioning
- https://platform.claude.com/docs/en/api/errors
- https://platform.claude.com/docs/en/api/rate-limits
- https://platform.claude.com/docs/en/agents-and-tools/tool-use/define-tools
- https://platform.claude.com/docs/en/about-claude/models/overview

（docs.claude.com 的舊網址會轉址到上面這些頁。）

| 項目 | 文件 | 實作 |
| --- | --- | --- |
| 端點 | `POST /v1/messages` | 登記的 base 加 `/v1/messages` |
| 標頭 | `x-api-key`、`anthropic-version`、`content-type: application/json` | 相同；版本固定為 `protocol::ANTHROPIC_VERSION = "2026-10-21"`（版本頁列為最新） |
| 串流事件 | `message_start`（usage.input_tokens）、`content_block_start`（text／tool_use）、`content_block_delta`（`text_delta`、`input_json_delta.partial_json`）、`content_block_stop`、`message_delta`（stop_reason、累計的 usage.output_tokens）、`message_stop`；另有 `ping` 與 `event: error` | 全部處理；不認得的事件略過；`message_stop` 沒到就算不完整（`malformed`） |
| stop_reason | `end_turn`、`max_tokens`、`stop_sequence`、`tool_use`、`pause_turn`、`refusal`、`model_context_window_exceeded` | `tool_use` → 執行工具；`max_tokens`、`model_context_window_exceeded` → run 以 `max_tokens` 結束；其他（含 `refusal`、`pause_turn`）→ 這一輪結束（`end_turn`）。**差異**：`pause_turn` 文件的意思是可以接著送；實作不續送（內建 AI 不用伺服器端工具，不會遇到） |
| 工具 | `tools[{name, description, input_schema}]`、`tool_choice` auto／any／tool／none | 送 `input_schema`；不送 `tool_choice`（預設 auto）。筆記：文件說新模型強制 any／tool 會回 400，所以不送是對的 |
| 錯誤 | 400、401、402、403、404、409、413、429、500、504、529（overloaded）；串流中的錯誤以 `event: error` 送 | 非 2xx → `ai_provider_error`（`http_status` 與狀態碼）；串流中的 error 事件 → `http_status`（依錯誤種類對應狀態碼，例如 overloaded_error 為 529；見 `protocol::anthropic_status`）；原文一律不保留 |
| usage | 有 `cache_creation_input_tokens`、`cache_read_input_tokens` | 只用 `input_tokens`、`output_tokens` 計價（沒用 prompt caching） |

## OpenAI 相容（Chat Completions）

來源：

- https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create
- https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events
- https://developers.openai.com/api/docs/guides/function-calling
- https://developers.openai.com/api/docs/guides/error-codes
- https://developers.openai.com/api/docs/guides/rate-limits

| 項目 | 文件 | 實作 |
| --- | --- | --- |
| 端點 | `POST /v1/chat/completions`，`Authorization: Bearer` | 登記的 base（含 `/v1`）加 `/chat/completions`；沒有 key 就不送 `Authorization` |
| 輸出上限 | `max_tokens` **已棄用**，改用 `max_completion_tokens`（也算 reasoning tokens；o 系列不收 `max_tokens`） | **差異**：實作送 `max_tokens`。自架伺服器（vLLM、Ollama）收 `max_tokens`；OpenAI 本身目前仍接受。若要直連 OpenAI 的 o 系列模型，要改送 `max_completion_tokens`（待使用者決定是否依 provider 切換） |
| usage | `stream_options: {include_usage: true}` 時，結束前多一個 `choices: []`、帶 `usage` 的 chunk；串流中斷時可能沒有 | 送 `include_usage`；沒收到就用估算（每字元 1 token，無條件進位），`usage_estimated = true`，結算不超過保留額 |
| 串流 | `chat.completion.chunk`；`choices[].delta.content`；`delta.tool_calls[]`（`index` 必有，`id`、`function.name`、`function.arguments` 分段）；`finish_reason` = `stop`／`length`／`tool_calls`／`content_filter`／`function_call`；以 `data: [DONE]` 結束 | 依 `index` 組裝工具呼叫；`length` → `max_tokens`；`tool_calls`、`function_call` → 執行工具；其他 → 這一輪結束；沒有 `[DONE]` 或 `finish_reason` 算不完整 |
| 工具 | `tools[{type: function, function: {name, description, parameters}}]`、`tool_choice`、`parallel_tool_calls` | 送 `parameters`；不送 `tool_choice`、`parallel_tool_calls`（用預設） |
| 錯誤 | 401、403（地區）、429（rate limit、額度）、500、503（overloaded）；串流開始後的錯誤在串流裡 | 非 2xx → `http_status`；串流裡帶 `error` 物件 → `http_status`（取 `error.code` 為數字時用它，否則 500） |

## 沒有確認的

- vLLM、Ollama 是否支援 `stream_options.include_usage`：文件沒寫（Ollama 的頁面當時 404）。不支援時會被當作沒回 usage → 估算。手動檢查時要看 `ai_runs.usage_estimated`。
- 自架伺服器對 `tools` 的支援依模型與啟動參數而定（vLLM 要開 tool parser）。「測試連線」會用一個 `ping` 工具試，結果寫在 `supports_tools`；不支援時對話不送工具。
