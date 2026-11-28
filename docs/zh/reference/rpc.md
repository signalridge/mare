# RPC 参考

`mare --rpc` 的全部方法、通知和回调，取自 `mare rpc schema`。协议字段统一使用 `snake_case`。会话如何经 RPC 运行以及示例见 [JSON-RPC](../integrate/rpc.md)。

> 本页由 `scripts/gen_docs.py` 从代码生成，请勿手工修改。说明文字来自代码，保持英文。

协议版本: **3.0**

## 方法（客户端 → mare）

### agent

#### `agent.cancel`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `clear_queue` | `boolean` |  |  |
| `mode` | `"cancel" \| "halt"` |  |  |
| `reason` | `string` |  | Cancellation cause (`user`, `parent`, `hook`, `disposed`) or free-form note (classified as `user`). |
| `request_id` | `string` |  |  |
| `turn_id` | `string` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `duplicate` | `boolean` |  |  |
| `removed` | `{ content?: object[], images: { data?: string, mime: string, path?: string, size?: integer }[], input_context?: any, input_id: string, lane: "steer" \| "follow_up", text: string }[]` | 是 |  |
| `turn_id` | `string` |  |  |

#### `agent.continue`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `effort` | `string` |  |  |
| `tools` | `string \| object[]` |  |  |
| `output_schema` | `object` |  |  |
| `source` | `string` |  |  |
| `system` | `string` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `turn_id` | `string` | 是 |  |

#### `agent.delete_queued`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `input_id` | `string` | 是 |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `object`

#### `agent.get`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `activity` | `{ extra: object, extra_errors: object }` |  |  |
| `busy` | `boolean` | 是 |  |
| `effort` | `string` |  |  |
| `interrupted` | `boolean` |  |  |
| `message_count` | `integer` |  |  |
| `model` | `string` |  |  |
| `queued` | `integer` | 是 |  |
| `session_id` | `string` |  |  |
| `turn_cancelled` | `boolean` |  |  |
| `turn_id` | `string` |  |  |

#### `agent.list_messages`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cursor` | `string` |  |  |
| `limit` | `integer` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `object[]` | 是 |  |
| `next_cursor` | `string` |  |  |

#### `agent.list_queue`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `{ content?: object[], images: { data?: string, mime: string, path?: string, size?: integer }[], input_context?: any, input_id: string, lane: "steer" \| "follow_up", text: string }[]` | 是 |  |

#### `agent.prompt`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `effort` | `string` |  |  |
| `when_busy` | `"queue" \| "reject"` |  |  |
| `input` | `string \| object[]` | 是 |  |
| `output_schema` | `object` |  |  |
| `request_id` | `string` |  |  |
| `source` | `string` |  |  |
| `system` | `string` |  |  |
| `tools` | `string \| object[]` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `duplicate` | `boolean` |  |  |
| `input_id` | `string` |  |  |
| `status` | `"started" \| "queued"` | 是 |  |
| `turn_id` | `string` |  |  |

#### `agent.steer`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `expected_turn_id` | `string` |  |  |
| `input` | `string \| object[]` | 是 |  |
| `request_id` | `string` |  |  |
| `source` | `string` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `duplicate` | `boolean` |  |  |
| `input_id` | `string` | 是 |  |

#### `agent.update_queued`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `input` | `string \| object[]` |  |  |
| `input_id` | `string` | 是 |  |
| `lane` | `"steer" \| "follow_up"` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `content` | `object[]` |  | Optional ordered content blocks with image bytes stripped (preserved on cancel). |
| `images` | `{ data?: string, mime: string, path?: string, size?: integer }[]` | 是 | Queued image attachments. |
| `input_context` | `any` |  | Opaque caller-supplied JSON metadata preserved with the queued input. |
| `input_id` | `string` | 是 | Stable queued input id. |
| `lane` | `"steer" \| "follow_up"` | 是 | Delivery lane (`steer` or `follow_up`). |
| `text` | `string` | 是 | Queued input text. |

#### `agent.wait`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `timeout_ms` | `integer` |  |  |
| `turn_id` | `string` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cancellation_cause` | `"user" \| "parent" \| "hook" \| "disposed"` |  | Origin of cancellation when `termination_reason` is `interrupted`. |
| `completed_at_ms` | `integer` |  | Unix epoch milliseconds when the turn finished. |
| `error` | `{ code: string, data?: any, message: string }` |  |  |
| `error_kind` | `string` |  | Classified failure kind when present. |
| `messages` | `object[]` |  | Session messages visible at turn end. |
| `not_sent` | `boolean` |  | True when the prompt failed before reaching the provider. |
| `save_warning` | `string` |  | Quarantine/save failure warning when another error took precedence. |
| `started_at_ms` | `integer` | 是 | Unix epoch milliseconds when the turn started. |
| `status` | `"completed" \| "cancelled" \| "failed" \| "running"` | 是 | Turn completion outcome (or `running` when `wait` timed out). |
| `stop_reason` | `string` |  | Final model stop reason when reported (e.g. `stop`, `length`, `filtered`). |
| `structured` | `any` |  | Validated structured output when `output_schema` was supplied. |
| `termination_reason` | `"completed" \| "usage_limit" \| "interrupted" \| "aborted" \| "turn_limit" \| "error"` |  | Machine-readable turn termination outcome. |
| `text` | `string` |  | Final assistant text produced during the turn. |
| `timed_out` | `boolean` |  | True when `agent.wait` returned on `timeout_ms` before settlement. |
| `truncated` | `boolean` |  | True when the final model response stopped at the output token limit. |
| `turn_id` | `string` | 是 | Turn identifier. |
| `usage` | `object` |  | Aggregated token/cost usage for the turn. |

### baseline

#### `baseline.read`

Parameters for `baseline.read`.

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `baseline_id` | `string` | 是 |  |
| `part` | `string` | 是 |  |
| `cursor` | `string \| null` |  |  |
| `limit` | `integer \| null` |  | Optional maximum number of items to return in one page (&gt;= 1). |

结果: `BaselineReadResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `any[]` | 是 |  |
| `next_cursor` | `string \| null` |  |  |

错误: `baseline.stale`, `baseline.unknown_part`, `baseline.item_too_large`

### cells

#### `cells.cancel`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cell_id` | `string` | 是 |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `object`

#### `cells.get`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `retains_state` | `boolean` | 是 |  |
| `running` | `integer` | 是 |  |
| `store_durable` | `boolean` | 是 |  |
| `stored_keys` | `integer` | 是 |  |

#### `cells.list`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cursor` | `string` |  |  |
| `limit` | `integer` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `{ cell_id: string, status: "running" \| "completed" \| "failed" \| "cancelled" }[]` | 是 |  |
| `next_cursor` | `string` |  |  |

#### `cells.list_tools`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cursor` | `string` |  |  |
| `limit` | `integer` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `{ declaration: string, description: string, name: string, output_schema: object, parameters: object }[]` | 是 |  |
| `next_cursor` | `string` |  |  |

#### `cells.reset`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `object`

#### `cells.start`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `capabilities` | `string[]` |  |  |
| `cell_id` | `string` |  |  |
| `code` | `string` | 是 |  |
| `max_output_tokens` | `integer` |  |  |
| `state_owner` | `string` |  |  |
| `timeout_ms` | `integer \| false` |  |  |
| `yield_time_ms` | `integer` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `mare.CellResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cell_id` | `string` | 是 |  |
| `content` | `any[]` | 是 |  |
| `error` | `string` |  |  |
| `result` | `any` |  |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled"` | 是 |  |
| `stop_unconfirmed` | `boolean` |  |  |

#### `cells.wait`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cell_id` | `string` | 是 |  |
| `max_output_tokens` | `integer` |  |  |
| `terminate` | `boolean` |  |  |
| `yield_time_ms` | `integer` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `mare.CellResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cell_id` | `string` | 是 |  |
| `content` | `any[]` | 是 |  |
| `error` | `string` |  |  |
| `result` | `any` |  |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled"` | 是 |  |
| `stop_unconfirmed` | `boolean` |  |  |

### commands

#### `commands.get`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | `string` | 是 |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `alias_of` | `string` |  |  |
| `args` | `string` |  |  |
| `auto_execute` | `boolean` |  |  |
| `availability` | `"always" \| "idle" \| "session" \| "main" \| "running" \| "tui" \| "always" \| "idle" \| "session" \| "main" \| "running" \| "tui"[]` |  |  |
| `description` | `string` |  |  |
| `local_only` | `boolean` |  |  |
| `name` | `string` | 是 |  |

#### `commands.list`

无参数。

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `object[]` | 是 |  |

#### `commands.register`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `alias_of` | `string` |  |  |
| `args` | `string` |  |  |
| `auto_execute` | `boolean` |  |  |
| `availability` | `"always" \| "idle" \| "session" \| "main" \| "running" \| "tui" \| "always" \| "idle" \| "session" \| "main" \| "running" \| "tui"[]` |  |  |
| `description` | `string` |  |  |
| `local_only` | `boolean` |  |  |
| `name` | `string` | 是 |  |
| `replace` | `boolean` |  |  |
| `callbacks` | `"complete" \| "run"[]` |  |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `registration_id` | `string` | 是 |  |

#### `commands.run`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `input` | `{ content?: object[] }` |  |  |
| `line` | `string` | 是 |  |
| `session_id` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `result` | `any` |  |  |

### compact

#### `compact.run`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `instructions` | `string` |  |  |
| `messages` | `object[]` |  |  |
| `reason` | `"manual"` |  |  |
| `summarize_only` | `boolean` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `dropped` | `integer` |  |  |
| `status` | `"compacted" \| "nothing_to_compact" \| "unchanged" \| "summarized"` | 是 |  |
| `summary` | `string` |  |  |
| `tokens_after` | `integer` | 是 |  |
| `tokens_before` | `integer` | 是 |  |

### config

#### `config.describe`

Return redacted config metadata, provenance, and redacted warnings.

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `keys` | `string[] \| null` |  | option names or dotted paths (default: all) |

结果: `ConfigDescribeResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `options` | `map&lt;string, OptionDescriptor&gt;` | 是 |  |
| `warnings` | `string[]` |  |  |

### connection

#### `connection.set_capabilities`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `capabilities` | `StrictClientCapabilities` | 是 |  |

结果: `SetCapabilitiesResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `capabilities` | `ClientCapabilities` | 是 |  |

### events

#### `events.list`

无参数。

结果: `EventsListResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `EventInfo[]` | 是 |  |

#### `events.subscribe`

`events.subscribe`: subscribe to (or filter) a session's event stream.

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string \| null` |  | session id to subscribe to (required unless `scope = "all"` is explicitly set by the owner) |
| `session_epoch` | `string \| null` |  | last observed `session_epoch`; mismatches fall back to `mode = "baseline"` |
| `last_seq` | `integer \| null` |  | last delivered `seq` within `session_epoch` for resumptive replay |
| `scope` | `RegistrationScopeKind \| null` |  |  |
| `include` | `string[] \| null` |  |  |
| `exclude` | `string[] \| null` |  |  |

结果: `EventsSubscribeResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `mode` | `SubscribeMode` | 是 |  |
| `session_epoch` | `string` | 是 |  |
| `seq` | `integer ≥ 0` | 是 | sequence number at which the stream continues (`next_seq` for `live`/`baseline`, `last_seq + 1` for `replay`) |

### flags

#### `flags.get`

`flags.get` params.

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | `string` | 是 |  |

结果: `FlagsGetResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | `string` | 是 |  |
| `registered` | `boolean` | 是 |  |
| `value` | `any` |  |  |

#### `flags.list`

无参数。

结果: `FlagsListResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `FlagEntry[]` | 是 |  |

### fs

#### `fs.list`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `path` | `string` | 是 |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `{ file_name: string, is_directory: boolean, is_file: boolean }[]` | 是 |  |

#### `fs.read`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `max_bytes` | `integer` |  |  |
| `path` | `string` | 是 |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data_base64` | `string` | 是 |  |

#### `fs.stat`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `path` | `string` | 是 |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `created_at_ms` | `integer` | 是 |  |
| `is_directory` | `boolean` | 是 |  |
| `is_file` | `boolean` | 是 |  |
| `mode` | `integer` |  |  |
| `modified_at_ms` | `integer` | 是 |  |
| `nlink` | `integer` |  |  |
| `size` | `integer` | 是 |  |

#### `fs.watch`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `path` | `string` | 是 |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `registration_id` | `string` | 是 |  |

#### `fs.write`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `atomic` | `boolean` |  |  |
| `create_new` | `boolean` |  |  |
| `data_base64` | `string` | 是 |  |
| `path` | `string` | 是 |  |

结果: `object`

### hooks

#### `hooks.subscribe`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `event` | `string` | 是 |  |
| `prepend` | `boolean` |  |  |
| `timeout_ms` | `integer` |  |  |
| `callbacks` | `"handle"[]` | 是 |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `registration_id` | `string` | 是 |  |

### `initialize`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `protocol_version` | `string` | 是 | Protocol version the host speaks (`major.minor`); the major version must match. |
| `client` | `ClientInfo \| null` |  |  |
| `capabilities` | `ClientCapabilities` |  | Callbacks the host can answer. Undeclared callbacks are never sent (defaults apply). (default `{"callbacks": false, "ui": false, "approvals": false, "elicitation": false}`) |
| `callback_timeout_ms` | `integer \| null` |  | Timeout for callbacks into the host in milliseconds (default 30000, clamped to 1..=3600000). |
| `approval_timeout_ms` | `integer \| null` |  | Optional human approval timeout in milliseconds; absent means wait without a deadline. |

结果: `InitializeResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `protocol_version` | `string` | 是 |  |
| `server` | `ServerInfo` | 是 |  |
| `capabilities` | `ServerCapabilities` | 是 |  |
| `session` | `any` |  | Live `mare.Session` snapshot of the opened initial session, if any (`session_id` and `session_epoch` live on this object; session-scoped contract methods take `session_id`). |

### jobs

#### `jobs.background`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `job_id` | `string` | 是 |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `object`

#### `jobs.cancel`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `job_id` | `string` | 是 |  |
| `reason` | `string` |  |  |
| `silent` | `boolean` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `object`

#### `jobs.list`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cursor` | `string` |  |  |
| `limit` | `integer` |  |  |
| `owner` | `string` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `object[]` | 是 |  |
| `next_cursor` | `string` |  |  |

#### `jobs.read`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `job_id` | `string` | 是 |  |
| `max_bytes` | `integer` |  |  |
| `offset` | `integer` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `done` | `boolean` |  |  |
| `dropped` | `integer` |  |  |
| `next_offset` | `integer` |  |  |
| `text` | `string` | 是 |  |

### llm

#### `llm.get_model`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `ref` | `string` |  | describe this model instead (hidden or unlisted ones too); the selection is unchanged |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `available` | `boolean` | 是 |  |
| `context_window` | `integer` |  |  |
| `cost` | `{ cache_read?: number, cache_write?: number, input?: number, output?: number }` |  |  |
| `default_effort` | `string` |  |  |
| `edit_tool` | `"apply_patch" \| "replace"` |  |  |
| `efforts` | `string[]` | 是 |  |
| `images` | `boolean` | 是 |  |
| `max_output` | `integer` |  |  |
| `model_id` | `string` | 是 |  |
| `name` | `string` |  |  |
| `parameters` | `object[]` | 是 |  |
| `provider` | `string` | 是 |  |
| `ready` | `boolean` | 是 |  |
| `ready_reason` | `string` |  |  |
| `reasoning` | `boolean` | 是 |  |
| `ref` | `string` | 是 |  |
| `route` | `string` |  |  |
| `route_error` | `string` |  |  |
| `tool_update` | `"addition_only"` |  |  |
| `unavailable_reason` | `string` |  |  |
| `wire_route` | `string` |  |  |

#### `llm.list_auth_keys`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `prefix` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `string[]` | 是 |  |

#### `llm.list_models`

无参数。

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `object[]` | 是 |  |

#### `llm.list_providers`

无参数。

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `object[]` | 是 |  |

#### `llm.login`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `key` | `string` |  |  |
| `method` | `"oauth" \| "device" \| "api_key" \| "auto"` |  |  |
| `provider` | `string` | 是 |  |

结果: `object`

#### `llm.logout`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `provider` | `string` | 是 |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `kept` | `string[]` | 是 |  |
| `removed` | `string[]` | 是 |  |
| `warnings` | `string[]` | 是 |  |

#### `llm.register_provider`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `default_model` | `string` |  |  |
| `models` | `object[]` |  |  |
| `provider` | `string` | 是 |  |
| `replace` | `boolean` |  |  |
| `callbacks` | `"stream"[]` |  |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `registration_id` | `string` | 是 |  |

#### `llm.set_effort`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `also_global` | `boolean` |  |  |
| `effort` | `string` | 是 |  |
| `save` | `boolean` |  |  |
| `scope_name` | `string` |  |  |
| `target` | `"session" \| "global"` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `object`

#### `llm.set_model`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `ref` | `string` | 是 |  |
| `save` | `boolean` |  |  |
| `target` | `"session" \| "global"` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `available` | `boolean` | 是 |  |
| `context_window` | `integer` |  |  |
| `cost` | `{ cache_read?: number, cache_write?: number, input?: number, output?: number }` |  |  |
| `default_effort` | `string` |  |  |
| `edit_tool` | `"apply_patch" \| "replace"` |  |  |
| `efforts` | `string[]` | 是 |  |
| `images` | `boolean` | 是 |  |
| `max_output` | `integer` |  |  |
| `model_id` | `string` | 是 |  |
| `name` | `string` |  |  |
| `parameters` | `object[]` | 是 |  |
| `provider` | `string` | 是 |  |
| `ready` | `boolean` | 是 |  |
| `ready_reason` | `string` |  |  |
| `reasoning` | `boolean` | 是 |  |
| `ref` | `string` | 是 |  |
| `route` | `string` |  |  |
| `route_error` | `string` |  |  |
| `tool_update` | `"addition_only"` |  |  |
| `unavailable_reason` | `string` |  |  |
| `wire_route` | `string` |  |  |

### mcp

#### `mcp.connect`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `server` | `string` | 是 |  |

结果: `mare.McpServerStatus`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `error` | `string` |  |  |
| `missing` | `string[]` |  |  |
| `name` | `string` | 是 |  |
| `source` | `string` |  |  |
| `state` | `string` | 是 |  |
| `target` | `string` |  |  |
| `tier` | `string` |  |  |
| `tools` | `integer` | 是 |  |
| `transport` | `string` |  |  |

#### `mcp.get_prompt`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `arguments` | `map&lt;string, string&gt;` |  |  |
| `name` | `string` | 是 |  |
| `server` | `string` | 是 |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `description` | `string` |  |  |
| `messages` | `object[]` | 是 |  |

#### `mcp.list_prompts`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `connect` | `boolean` |  |  |
| `server` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `object[]` | 是 |  |

#### `mcp.list_resources`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `connect` | `boolean` |  |  |
| `server` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `object[]` | 是 |  |
| `resource_templates` | `object[]` | 是 |  |

#### `mcp.list_servers`

无参数。

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `mare.McpServerStatus[]` | 是 |  |
| `errors` | `string[]` | 是 |  |

#### `mcp.list_tools`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `connect` | `boolean` |  |  |
| `server` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `object[]` | 是 |  |

#### `mcp.read_resource`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `server` | `string` | 是 |  |
| `uri` | `string` | 是 |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `contents` | `object[]` | 是 |  |

#### `mcp.reconnect`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `server` | `string` | 是 |  |

结果: `mare.McpServerStatus`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `error` | `string` |  |  |
| `missing` | `string[]` |  |  |
| `name` | `string` | 是 |  |
| `source` | `string` |  |  |
| `state` | `string` | 是 |  |
| `target` | `string` |  |  |
| `tier` | `string` |  |  |
| `tools` | `integer` | 是 |  |
| `transport` | `string` |  |  |

### message

#### `message.append`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `content` | `string \| any[]` | 是 |  |
| `created_at_ms` | `integer` |  |  |
| `delivery` | `{ deliver_as?: "steer" \| "follow_up" \| "next_turn", trigger_turn?: boolean, wait?: boolean }` |  |  |
| `details` | `any` |  |  |
| `display` | `boolean` |  |  |
| `request_id` | `string` |  |  |
| `type` | `string` | 是 |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `duplicate` | `boolean` |  |  |
| `message` | `object` | 是 |  |
| `result` | `object` |  |  |
| `status` | `"appended" \| "queued" \| "deferred" \| "triggered"` | 是 |  |

#### `message.list_types`

无参数。

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `{ type: string }[]` | 是 |  |

#### `message.register_type`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `type` | `string` | 是 |  |
| `callbacks` | `"render" \| "validate"[]` |  |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `registration_id` | `string` | 是 |  |

### options

#### `options.get`

Read effective option values (matching `mare.o.<name>`). Sensitive fields (`config_sensitive`, `providers` keys/headers/query, credentials) are always redacted like `/config`.

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |
| `keys` | `string[] \| null` |  | option names or dotted paths (default: all) |
| `describe` | `boolean` |  | return per-key metadata (`effective`, `default`, `source`, `description`, `redacted`, …) instead of bare values (default `false`) |

结果: `object`

#### `options.set`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |
| `values` | `object` | 是 | name → value (e.g. `{"model": "openai/gpt-6.1-terra", "effort": "high"}`) |

结果: `EmptyResult`

### peer

#### `peer.info`

无参数。

结果: `PeerInfo`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | `string` | 是 |  |
| `pid` | `integer ≥ 0` | 是 |  |
| `cwd` | `string` | 是 |  |
| `version` | `string` | 是 | mare version |
| `allowed` | `string[]` | 是 | methods this connection may call (default set + the session's `peer.allow`) |
| `delivery_modes` | `string[]` |  | supported inbound message delivery modes (e.g. `trigger_turn`, `queue_only`) |

#### `peer.message`

`peer.message` params: a message from another session. Where the socket reports the
connecting process (Unix credentials), a `from` that disagrees with it is refused: a `pid`
other than the connecting process's, or the `name` of a registered session that isn't it.
Everything else in `from`, and `policy`, is self-reported: shown only, never grounds for
allowing anything. Always held until the receiving user confirms; a plugin can
take it over via the `peer/inbound` event. Never an approval.

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `from` | `PeerRef` |  | (default `{"name": null, "pid": null, "cwd": null, "session_id": null}`) |
| `text` | `string` | 是 |  |
| `queue_only` | `boolean` |  | enqueue the message without waking an idle receiver (default `false`) |
| `policy` | `any` |  | the sender's policy as it reports it (display only until identities can be verified) |

结果: `PeerMessageResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `message_id` | `string` | 是 | id of this message (repeated in `peer.message_status`) |
| `status` | `string` | 是 | "held" (waiting for the receiving user; the outcome follows as `peer.message_status`) or "handled" (a plugin took it over via `peer/inbound`) |

### permissions

#### `permissions.get`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `desc` | `string` |  |  |
| `grants` | `{ network?: boolean \| string[], read?: string[], write?: string[] }` |  |  |
| `label` | `string` | 是 |  |
| `level` | `string` |  |  |
| `like` | `"read-only" \| "auto" \| "full-access"` |  |  |
| `partial` | `boolean` | 是 |  |
| `policy_revision` | `integer` | 是 |  |
| `preset` | `string` | 是 |  |
| `rules` | `object[]` |  |  |

#### `permissions.list`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `{ current: boolean, desc: string, label: string, level: string, like: "read-only" \| "auto" \| "full-access", name: string }[]` | 是 |  |

#### `permissions.set`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `confirm` | `boolean` |  |  |
| `name` | `string` | 是 |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `desc` | `string` |  |  |
| `grants` | `{ network?: boolean \| string[], read?: string[], write?: string[] }` |  |  |
| `label` | `string` | 是 |  |
| `level` | `string` |  |  |
| `like` | `"read-only" \| "auto" \| "full-access"` |  |  |
| `partial` | `boolean` | 是 |  |
| `policy_revision` | `integer` | 是 |  |
| `preset` | `string` | 是 |  |
| `rules` | `object[]` |  |  |

### plugins

#### `plugins.check`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `names` | `string[]` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `{ behind_count: integer, breaking: boolean, commits: string[], error?: string, name: string, status: "up_to_date" \| "outdated" \| "pinned" \| "local" \| "unreachable" }[]` | 是 |  |

#### `plugins.install`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | `string` |  | Optional plugin short name or declared source name; omitted operates on all eligible sources. |
| `reload` | `boolean` |  | When true and the package operation succeeds, run a full host reload. |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `log` | `string[]` | 是 |  |
| `reload` | `{ added: string[], notices: string[], reloaded: string[], removed: string[], replaced: string[] }` |  |  |

#### `plugins.list`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `kind` | `"skills" \| "prompts" \| "themes" \| "types"` |  | Optional content kind (`skills \| prompts \| themes \| types`) for `dirs`; omitted returns package roots. |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `object[]` | 是 |  |
| `diagnostics` | `{ constraint?: string, message: string, path: string, plugin: string }[]` | 是 |  |
| `dirs` | `{ dir: string, name: string }[]` | 是 |  |

#### `plugins.uninstall`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | `string` |  | Optional plugin short name or declared source name; omitted operates on all eligible sources. |
| `reload` | `boolean` |  | When true and the package operation succeeds, run a full host reload. |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `log` | `string[]` | 是 |  |
| `reload` | `{ added: string[], notices: string[], reloaded: string[], removed: string[], replaced: string[] }` |  |  |

#### `plugins.update`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | `string` |  | Optional plugin short name or declared source name; omitted operates on all eligible sources. |
| `reload` | `boolean` |  | When true and the package operation succeeds, run a full host reload. |
| `stash` | `boolean` |  | Save local changes and untracked files in git stash before updating. |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `log` | `string[]` | 是 |  |
| `reload` | `{ added: string[], notices: string[], reloaded: string[], removed: string[], replaced: string[] }` |  |  |

### query

#### `query.call`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `arguments` | `object` | 是 |  |
| `operation` | `string` | 是 |  |
| `provider` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `next_cursor` | `string` |  |  |
| `provider` | `string` |  |  |

#### `query.list_providers`

无参数。

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `{ name: string, operations: string[], priority: integer, revision: string, source: { capability: "in_process" \| "opaque" } }[]` | 是 |  |

### registrations

#### `registrations.release`

Release one registration this connection holds; answered once it is withdrawn.

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `registration_id` | `string` | 是 |  |

结果: `ReleaseResult`

### services

#### `services.list`

无参数。

结果: `ServicesListResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `ServiceInfo[]` | 是 |  |

#### `services.register`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | `string` | 是 |  |
| `methods` | `map&lt;string, MethodSpec&gt;` | 是 |  |
| `scope` | `RegistrationScopeKind \| null` |  |  |
| `session_id` | `string \| null` |  |  |

结果: `Registration`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `registration_id` | `string` | 是 |  |
| `name` | `string` | 是 |  |

### session

#### `session.add_record`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `any` |  |  |
| `kind` | `string` | 是 |  |
| `persist` | `boolean` |  |  |
| `sync` | `boolean` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `durable` | `boolean` | 是 |  |
| `record_id` | `string` | 是 |  |

#### `session.archive`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |

结果: `object`

#### `session.close`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `expected_epoch` | `string` |  |  |
| `session_id` | `string` | 是 |  |

结果: `object`

#### `session.create`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cwd` | `string` |  |  |
| `name` | `string` |  |  |
| `request_id` | `string` |  |  |
| `subscribe` | `boolean` |  |  |

结果: `mare.LiveSession`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `archived` | `boolean` |  | Whether the session is archived in the index. |
| `created_at_ms` | `integer` | 是 | Session creation timestamp in milliseconds. |
| `cwd` | `string` | 是 | Working directory of the session. |
| `ephemeral` | `boolean` |  | Whether the session is running with persistence disabled. |
| `leaf_entry_id` | `string` |  | Current leaf entry id when loaded. |
| `loaded` | `boolean` | 是 | Whether this session is loaded in the current host. |
| `name` | `string` |  | Optional user-assigned or generated session name. |
| `name_revision` | `integer` |  | Monotonic revision of the session name when loaded. |
| `parent_session_id` | `string` |  | Parent or fork-origin session id, if any. |
| `path` | `string` |  | On-disk JSONL file path when saved. |
| `session_epoch` | `string` | 是 | In-memory session epoch when loaded. |
| `session_id` | `string` | 是 | Unique session id. |
| `status` | `"idle" \| "busy"` |  | Live activity status when loaded. |
| `updated_at_ms` | `integer` | 是 | Last update timestamp in milliseconds. |

#### `session.delete`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |

结果: `object`

#### `session.fork`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `before_item_id` | `string` |  |  |
| `expected_epoch` | `string` |  |  |
| `name` | `string` |  |  |
| `request_id` | `string` |  |  |
| `session_id` | `string` | 是 |  |
| `subscribe` | `boolean` |  |  |

结果: `mare.LiveSession`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `archived` | `boolean` |  | Whether the session is archived in the index. |
| `created_at_ms` | `integer` | 是 | Session creation timestamp in milliseconds. |
| `cwd` | `string` | 是 | Working directory of the session. |
| `ephemeral` | `boolean` |  | Whether the session is running with persistence disabled. |
| `leaf_entry_id` | `string` |  | Current leaf entry id when loaded. |
| `loaded` | `boolean` | 是 | Whether this session is loaded in the current host. |
| `name` | `string` |  | Optional user-assigned or generated session name. |
| `name_revision` | `integer` |  | Monotonic revision of the session name when loaded. |
| `parent_session_id` | `string` |  | Parent or fork-origin session id, if any. |
| `path` | `string` |  | On-disk JSONL file path when saved. |
| `session_epoch` | `string` | 是 | In-memory session epoch when loaded. |
| `session_id` | `string` | 是 | Unique session id. |
| `status` | `"idle" \| "busy"` |  | Live activity status when loaded. |
| `updated_at_ms` | `integer` | 是 | Last update timestamp in milliseconds. |

#### `session.get`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |

结果: `mare.Session`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `archived` | `boolean` |  | Whether the session is archived in the index. |
| `created_at_ms` | `integer` | 是 | Session creation timestamp in milliseconds. |
| `cwd` | `string` | 是 | Working directory of the session. |
| `ephemeral` | `boolean` |  | Whether the session is running with persistence disabled. |
| `leaf_entry_id` | `string` |  | Current leaf entry id when loaded. |
| `loaded` | `boolean` | 是 | Whether this session is loaded in the current host. |
| `name` | `string` |  | Optional user-assigned or generated session name. |
| `name_revision` | `integer` |  | Monotonic revision of the session name when loaded. |
| `parent_session_id` | `string` |  | Parent or fork-origin session id, if any. |
| `path` | `string` |  | On-disk JSONL file path when saved. |
| `session_epoch` | `string` |  | In-memory session epoch when loaded. |
| `session_id` | `string` | 是 | Unique session id. |
| `source_leaf_entry_id` | `string` |  | Source leaf entry id observed when creating a snapshot, if any. |
| `status` | `"idle" \| "busy"` |  | Live activity status when loaded. |
| `updated_at_ms` | `integer` | 是 | Last update timestamp in milliseconds. |

#### `session.get_item`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `after` | `integer` |  |  |
| `before` | `integer` |  |  |
| `item_id` | `string` | 是 |  |
| `max_bytes` | `integer` |  |  |
| `offset` | `integer` |  |  |
| `session_id` | `string` | 是 |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `chunk` | `string` |  |  |
| `chunk_offset` | `integer` |  |  |
| `corpus_revision` | `string` | 是 |  |
| `item` | `object` |  |  |
| `item_id` | `string` | 是 |  |
| `neighbors` | `{ item_id: string, type: string }[]` | 是 |  |
| `next_offset` | `integer` |  |  |
| `seq` | `integer` | 是 |  |
| `session_id` | `string` | 是 |  |
| `total_bytes` | `integer` |  |  |

#### `session.label_item`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `item_id` | `string` |  |  |
| `label` | `string` | 是 |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `item_id` | `string` | 是 |  |
| `label` | `string` |  |  |

#### `session.list`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `all` | `boolean` |  |  |
| `archived` | `boolean` |  |  |
| `cursor` | `string` |  |  |
| `cwd` | `string` |  |  |
| `limit` | `integer` |  |  |
| `loaded` | `boolean` |  |  |
| `parent_session_id` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `mare.Session[]` | 是 |  |
| `next_cursor` | `string` |  |  |

#### `session.list_items`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `branch` | `"current" \| "all"` |  |  |
| `cursor` | `string` |  |  |
| `direction` | `"asc" \| "desc"` |  |  |
| `limit` | `integer` |  |  |
| `types` | `string[]` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `object[]` | 是 |  |
| `leaf_entry_id` | `string` |  |  |
| `next_cursor` | `string` |  |  |

#### `session.list_records`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `branch` | `"current" \| "all"` |  |  |
| `cursor` | `string` |  |  |
| `kind` | `string` | 是 |  |
| `limit` | `integer` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `any[]` | 是 |  |
| `next_cursor` | `string` |  |  |

#### `session.load`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `path` | `string` |  |  |
| `session_id` | `string` |  |  |
| `subscribe` | `boolean` |  |  |

结果: `mare.LiveSession`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `archived` | `boolean` |  | Whether the session is archived in the index. |
| `created_at_ms` | `integer` | 是 | Session creation timestamp in milliseconds. |
| `cwd` | `string` | 是 | Working directory of the session. |
| `ephemeral` | `boolean` |  | Whether the session is running with persistence disabled. |
| `leaf_entry_id` | `string` |  | Current leaf entry id when loaded. |
| `loaded` | `boolean` | 是 | Whether this session is loaded in the current host. |
| `name` | `string` |  | Optional user-assigned or generated session name. |
| `name_revision` | `integer` |  | Monotonic revision of the session name when loaded. |
| `parent_session_id` | `string` |  | Parent or fork-origin session id, if any. |
| `path` | `string` |  | On-disk JSONL file path when saved. |
| `session_epoch` | `string` | 是 | In-memory session epoch when loaded. |
| `session_id` | `string` | 是 | Unique session id. |
| `status` | `"idle" \| "busy"` |  | Live activity status when loaded. |
| `updated_at_ms` | `integer` | 是 | Last update timestamp in milliseconds. |

#### `session.rewind`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `before_item_id` | `string` |  |  |
| `expected_epoch` | `string` |  |  |
| `restore_files` | `boolean` |  |  |
| `session_id` | `string` | 是 |  |
| `summarize` | `boolean` |  |  |

结果: `mare.LiveSession`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `archived` | `boolean` |  | Whether the session is archived in the index. |
| `created_at_ms` | `integer` | 是 | Session creation timestamp in milliseconds. |
| `cwd` | `string` | 是 | Working directory of the session. |
| `ephemeral` | `boolean` |  | Whether the session is running with persistence disabled. |
| `leaf_entry_id` | `string` |  | Current leaf entry id when loaded. |
| `loaded` | `boolean` | 是 | Whether this session is loaded in the current host. |
| `name` | `string` |  | Optional user-assigned or generated session name. |
| `name_revision` | `integer` |  | Monotonic revision of the session name when loaded. |
| `parent_session_id` | `string` |  | Parent or fork-origin session id, if any. |
| `path` | `string` |  | On-disk JSONL file path when saved. |
| `session_epoch` | `string` | 是 | In-memory session epoch when loaded. |
| `session_id` | `string` | 是 | Unique session id. |
| `status` | `"idle" \| "busy"` |  | Live activity status when loaded. |
| `updated_at_ms` | `integer` | 是 | Last update timestamp in milliseconds. |

#### `session.search`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cursor` | `string` |  |  |
| `event_filters` | `{ event_types?: string[], from_ms?: integer, seq_from?: integer, seq_to?: integer, surfaces?: "current" \| "shadowed" \| "log_only"[], to_ms?: integer }` |  |  |
| `filters` | `object` |  |  |
| `limit` | `integer` |  |  |
| `query` | `string` |  |  |
| `sort_direction` | `"asc" \| "desc"` |  |  |
| `sort_key` | `"relevance" \| "created_at_ms" \| "updated_at_ms"` |  |  |
| `step_bound` | `boolean` |  |  |
| `title_query` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `corpus_revision` | `string` | 是 |  |
| `data` | `object[]` | 是 |  |
| `incomplete` | `boolean` | 是 |  |
| `next_cursor` | `string` |  |  |

#### `session.search_items`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cursor` | `string` |  |  |
| `event_filters` | `{ event_types?: string[], from_ms?: integer, seq_from?: integer, seq_to?: integer, surfaces?: "current" \| "shadowed" \| "log_only"[], to_ms?: integer }` |  |  |
| `filters` | `object` |  |  |
| `limit` | `integer` |  |  |
| `query` | `string` |  |  |
| `session_id` | `string` | 是 |  |
| `sort_direction` | `"asc" \| "desc"` |  |  |
| `sort_key` | `"relevance" \| "created_at_ms" \| "updated_at_ms"` |  |  |
| `step_bound` | `boolean` |  |  |
| `title_query` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `corpus_revision` | `string` | 是 |  |
| `data` | `object[]` | 是 |  |
| `incomplete` | `boolean` | 是 |  |
| `next_cursor` | `string` |  |  |

#### `session.snapshot`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `before_item_id` | `string` |  |  |
| `effort` | `string` |  |  |
| `expected_epoch` | `string` |  |  |
| `initiating_tool_call_id` | `string` |  |  |
| `last_completed_turns` | `integer` |  |  |
| `model` | `string` |  |  |
| `name` | `string` |  |  |
| `request_id` | `string` |  |  |
| `session_id` | `string` | 是 |  |

结果: `mare.Session`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `archived` | `boolean` |  | Whether the session is archived in the index. |
| `created_at_ms` | `integer` | 是 | Session creation timestamp in milliseconds. |
| `cwd` | `string` | 是 | Working directory of the session. |
| `ephemeral` | `boolean` |  | Whether the session is running with persistence disabled. |
| `leaf_entry_id` | `string` |  | Current leaf entry id when loaded. |
| `loaded` | `boolean` | 是 | Whether this session is loaded in the current host. |
| `name` | `string` |  | Optional user-assigned or generated session name. |
| `name_revision` | `integer` |  | Monotonic revision of the session name when loaded. |
| `parent_session_id` | `string` |  | Parent or fork-origin session id, if any. |
| `path` | `string` |  | On-disk JSONL file path when saved. |
| `session_epoch` | `string` |  | In-memory session epoch when loaded. |
| `session_id` | `string` | 是 | Unique session id. |
| `source_leaf_entry_id` | `string` |  | Source leaf entry id observed when creating a snapshot, if any. |
| `status` | `"idle" \| "busy"` |  | Live activity status when loaded. |
| `updated_at_ms` | `integer` | 是 | Last update timestamp in milliseconds. |

#### `session.trace`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `ancestors` | `{ created_at_ms?: integer, session_id: string, title_available: boolean }[]` | 是 |  |
| `complete` | `boolean` | 是 |  |
| `descendants` | `object[]` | 是 |  |
| `incomplete` | `boolean` | 是 |  |
| `metadata_only` | `boolean` | 是 |  |
| `parent_boundary` | `boolean` | 是 |  |
| `root` | `{ created_at_ms?: integer, session_id: string, title_available: boolean }` |  |  |
| `scope` | `string` | 是 |  |
| `target` | `{ created_at_ms?: integer, session_id: string, title_available: boolean }` | 是 |  |

#### `session.trace_item`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `item_id` | `string` | 是 |  |
| `session_id` | `string` | 是 |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `availability` | `string` | 是 |  |
| `created_at_ms` | `integer` |  |  |
| `derived_event_seqs` | `integer[]` | 是 |  |
| `entry_id` | `string` |  |  |
| `incomplete` | `boolean` | 是 |  |
| `item_id` | `string` | 是 |  |
| `replaced_by` | `integer` |  |  |
| `replaced_event_seqs` | `integer[]` | 是 |  |
| `replacement_chain` | `integer[]` | 是 |  |
| `result_entry_id` | `string` |  |  |
| `role` | `string` |  |  |
| `seq` | `integer` | 是 |  |
| `session_id` | `string` | 是 |  |
| `source_event_seqs` | `integer[]` | 是 |  |
| `summary` | `string` | 是 |  |
| `surface` | `string` | 是 |  |
| `type` | `string` | 是 |  |

#### `session.unarchive`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |

结果: `object`

#### `session.update`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `expected_revision` | `integer` |  |  |
| `name` | `string` | 是 |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `mare.LiveSession`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `archived` | `boolean` |  | Whether the session is archived in the index. |
| `created_at_ms` | `integer` | 是 | Session creation timestamp in milliseconds. |
| `cwd` | `string` | 是 | Working directory of the session. |
| `ephemeral` | `boolean` |  | Whether the session is running with persistence disabled. |
| `leaf_entry_id` | `string` |  | Current leaf entry id when loaded. |
| `loaded` | `boolean` | 是 | Whether this session is loaded in the current host. |
| `name` | `string` |  | Optional user-assigned or generated session name. |
| `name_revision` | `integer` |  | Monotonic revision of the session name when loaded. |
| `parent_session_id` | `string` |  | Parent or fork-origin session id, if any. |
| `path` | `string` |  | On-disk JSONL file path when saved. |
| `session_epoch` | `string` | 是 | In-memory session epoch when loaded. |
| `session_id` | `string` | 是 | Unique session id. |
| `status` | `"idle" \| "busy"` |  | Live activity status when loaded. |
| `updated_at_ms` | `integer` | 是 | Last update timestamp in milliseconds. |

### `shutdown`

无参数。

结果: `EmptyResult`

### spill

#### `spill.delete`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `spill_id` | `string` | 是 |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `object`

#### `spill.list_backends`

无参数。

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `{ name: string, selected: boolean }[]` | 是 |  |

#### `spill.read`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `encoding` | `"text" \| "base64"` |  |  |
| `limit` | `integer` |  |  |
| `offset` | `integer` |  |  |
| `spill_id` | `string` | 是 |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `content` | `string` |  |  |
| `data_base64` | `string` |  |  |
| `next_offset` | `integer` |  |  |
| `size` | `integer` |  |  |

#### `spill.save`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `backend` | `string` |  |  |
| `content` | `string` |  |  |
| `data_base64` | `string` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `size` | `integer` | 是 |  |
| `spill_id` | `string` | 是 |  |

### state

#### `state.compare_exchange`

Atomically replace durable namespace state at `expected_revision` (`ok`, `conflict` with the current snapshot, or `rejected`).
Strict JSON preserves null and empty containers; deletion requires `null` and retains a monotonic revision. Cancellation before publication prevents writes; after publication admission rollback is not guaranteed, and `unknown_state` requires reread/reconciliation.

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `namespace` | `string` | 是 |  |
| `expected_revision` | `integer ≥ 0` | 是 |  |
| `value` | `any` | 是 |  |
| `delete` | `boolean` |  | (default `false`) |

结果: `StateCompareExchangeResult`

#### `state.snapshot`

Read isolated durable application state (distinct from `agent.get`).
Absent or deleted namespaces report `exists: false` and `null` value with tombstone revision retained.

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `namespace` | `string` | 是 |  |

结果: `StateSnapshotResult`

### status

#### `status.get`

无参数。

结果: `mare.Status`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cwd` | `string` | 是 |  |
| `message` | `string` | 是 |  |
| `model` | `string` |  |  |
| `pid` | `integer` | 是 |  |
| `seq` | `integer` | 是 |  |
| `session` | `{ path?: string, resume?: string[], session_id?: string }` | 是 |  |
| `state` | `"idle" \| "working" \| "blocked"` | 是 |  |

### subagent

#### `subagent.cancel`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `subagent_id` | `string` | 是 |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `object`

#### `subagent.get`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `subagent_id` | `string` | 是 |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `ended_at_ms` | `integer` |  |  |
| `error` | `{ code: string, data?: any, message: string }` |  |  |
| `error_kind` | `string` |  |  |
| `label` | `string` |  |  |
| `name` | `string` |  |  |
| `parent_session_id` | `string` |  |  |
| `run_id` | `string` |  |  |
| `seq` | `integer` |  |  |
| `started_at_ms` | `integer` | 是 |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled" \| "interrupted"` | 是 |  |
| `subagent_id` | `string` | 是 | Unique identifier (child session id) of the subagent. |
| `task` | `string` |  |  |

#### `subagent.list`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cursor` | `string` |  |  |
| `limit` | `integer` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `object[]` | 是 |  |
| `next_cursor` | `string` |  |  |

#### `subagent.list_types`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `object[]` | 是 |  |

#### `subagent.register_type`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `definition` | `object` | 是 |  |
| `replace` | `boolean` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `registration_id` | `string` | 是 |  |

#### `subagent.start`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `config` | `object` |  |  |
| `cwd` | `string` |  |  |
| `effort` | `string` |  |  |
| `fork` | `"none" \| "all" \| integer` |  |  |
| `label` | `string` |  |  |
| `messages` | `object[]` |  |  |
| `model` | `string` |  |  |
| `name` | `string` |  |  |
| `output_schema` | `object` |  |  |
| `params` | `object` |  |  |
| `persona` | `string` |  |  |
| `phase` | `string` |  |  |
| `prompt` | `string` | 是 |  |
| `run_id` | `string` |  |  |
| `sections` | `object[]` |  |  |
| `seq` | `integer` |  |  |
| `system` | `string` |  |  |
| `task` | `string` |  |  |
| `tools` | `string[]` |  |  |
| `type` | `string` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `ended_at_ms` | `integer` |  |  |
| `error` | `{ code: string, data?: any, message: string }` |  |  |
| `error_kind` | `string` |  |  |
| `label` | `string` |  |  |
| `name` | `string` |  |  |
| `parent_session_id` | `string` |  |  |
| `run_id` | `string` |  |  |
| `seq` | `integer` |  |  |
| `started_at_ms` | `integer` | 是 |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled" \| "interrupted"` | 是 |  |
| `subagent_id` | `string` | 是 | Unique identifier (child session id) of the subagent. |
| `task` | `string` |  |  |

#### `subagent.steer`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `subagent_id` | `string` | 是 |  |
| `text` | `string` | 是 |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `object`

#### `subagent.wait`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `subagent_id` | `string` | 是 |  |
| `timeout_ms` | `integer` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `error` | `{ code: string, data?: any, message: string }` |  |  |
| `error_kind` | `string` |  |  |
| `messages` | `object[]` |  |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled" \| "interrupted"` | 是 |  |
| `stop_reason` | `string` |  |  |
| `subagent_id` | `string` | 是 | Unique identifier (child session id) of the subagent. |
| `text` | `string` |  |  |
| `timed_out` | `boolean` | 是 |  |
| `truncated` | `boolean` |  |  |
| `usage` | `object` |  |  |

### terminal

#### `terminal.close`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `grace_ms` | `integer` |  | Optional SIGTERM grace period in milliseconds (0..30000, default 3000). |
| `terminal_id` | `string` | 是 | Owned terminal identifier returned by `terminal.create`. |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `mare.TerminalSessionSnapshot`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `alternate_screen` | `boolean` |  |  |
| `cols` | `integer` | 是 |  |
| `command_exit_code` | `integer` |  |  |
| `exit_code` | `integer` |  |  |
| `exit_signal` | `integer` |  |  |
| `name` | `string` |  |  |
| `output_offset` | `integer` | 是 |  |
| `pid` | `integer` |  |  |
| `prompt_seq` | `integer` |  |  |
| `rows` | `integer` | 是 |  |
| `screen` | `string` |  |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled"` | 是 |  |
| `terminal_id` | `string` | 是 |  |
| `truncated` | `boolean` | 是 |  |
| `view_truncated` | `boolean` |  |  |

#### `terminal.create`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `args` | `string[]` |  |  |
| `buffer_bytes` | `integer` |  |  |
| `cmd` | `string` |  |  |
| `cols` | `integer` |  | Terminal columns (1..500, default 80). |
| `cwd` | `string` |  |  |
| `emulate` | `boolean` |  |  |
| `env` | `map&lt;string, string&gt;` |  |  |
| `name` | `string` |  | Optional unique name within this owner (1..128 bytes). |
| `rows` | `integer` |  | Terminal rows (1..200, default 24). |
| `scrollback_lines` | `integer` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `mare.TerminalSessionSnapshot`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `alternate_screen` | `boolean` |  |  |
| `cols` | `integer` | 是 |  |
| `command_exit_code` | `integer` |  |  |
| `exit_code` | `integer` |  |  |
| `exit_signal` | `integer` |  |  |
| `name` | `string` |  |  |
| `output_offset` | `integer` | 是 |  |
| `pid` | `integer` |  |  |
| `prompt_seq` | `integer` |  |  |
| `rows` | `integer` | 是 |  |
| `screen` | `string` |  |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled"` | 是 |  |
| `terminal_id` | `string` | 是 |  |
| `truncated` | `boolean` | 是 |  |
| `view_truncated` | `boolean` |  |  |

#### `terminal.list`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `mare.TerminalSessionSnapshot[]` | 是 |  |

#### `terminal.read`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `count` | `integer` |  |  |
| `max_bytes` | `integer` |  | Maximum output bytes to return (1..4194304, default 262144). Applies to both incremental `output_offset` slicing (UTF-8 safe, floor one char) and VT page mode as well as `screen`; sets `truncated` true when capped. |
| `offset` | `integer` |  |  |
| `output_offset` | `integer` |  |  |
| `screen` | `boolean` |  |  |
| `terminal_id` | `string` | 是 | Owned terminal identifier returned by `terminal.create`. |
| `wait_ms` | `integer` |  | Optional bounded wait in milliseconds (0..60000). |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `alternate_screen` | `boolean` |  |  |
| `cols` | `integer` | 是 |  |
| `command_exit_code` | `integer` |  |  |
| `exit_code` | `integer` |  |  |
| `exit_signal` | `integer` |  |  |
| `line_begin` | `integer` | 是 |  |
| `line_end` | `integer` | 是 |  |
| `name` | `string` |  |  |
| `next_output_offset` | `integer` | 是 |  |
| `output_offset` | `integer` | 是 |  |
| `pid` | `integer` |  |  |
| `prompt_seq` | `integer` |  |  |
| `rows` | `integer` | 是 |  |
| `screen` | `string` |  |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled"` | 是 |  |
| `terminal_id` | `string` | 是 |  |
| `text` | `string` | 是 |  |
| `total_lines` | `integer` | 是 |  |
| `truncated` | `boolean` | 是 |  |
| `view_truncated` | `boolean` |  |  |

#### `terminal.resize`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cols` | `integer` | 是 | Terminal columns (1..500, default 80). |
| `rows` | `integer` | 是 | Terminal rows (1..200, default 24). |
| `terminal_id` | `string` | 是 | Owned terminal identifier returned by `terminal.create`. |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `object`

#### `terminal.send`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `max_bytes` | `integer` |  | Maximum output bytes to return (1..4194304, default 262144). Output is truncated at a UTF-8 boundary (always returning at least one character if non-empty) and sets `truncated` true; when `screen` is true, rendered screen is head-trimmed to fit and sets `view_truncated`. |
| `screen` | `boolean` |  |  |
| `submit` | `boolean` |  |  |
| `terminal_id` | `string` | 是 | Owned terminal identifier returned by `terminal.create`. |
| `text` | `string` |  |  |
| `wait_ms` | `integer` |  | Optional bounded wait in milliseconds (0..60000). |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `alternate_screen` | `boolean` |  |  |
| `cols` | `integer` | 是 |  |
| `command_exit_code` | `integer` |  |  |
| `exit_code` | `integer` |  |  |
| `exit_signal` | `integer` |  |  |
| `name` | `string` |  |  |
| `next_output_offset` | `integer` | 是 |  |
| `output` | `string` | 是 |  |
| `output_offset` | `integer` | 是 |  |
| `pid` | `integer` |  |  |
| `prompt_seq` | `integer` |  |  |
| `rows` | `integer` | 是 |  |
| `screen` | `string` |  |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled"` | 是 |  |
| `terminal_id` | `string` | 是 |  |
| `timed_out` | `boolean` | 是 |  |
| `truncated` | `boolean` | 是 |  |
| `view_truncated` | `boolean` |  |  |

#### `terminal.signal`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `signal` | `"SIGINT" \| "SIGTERM" \| "SIGKILL" \| "SIGTSTP" \| "SIGHUP"` | 是 |  |
| `terminal_id` | `string` | 是 | Owned terminal identifier returned by `terminal.create`. |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `object`

### theme

#### `theme.cancel_preview`

无参数。

结果: `mare.ThemeSnapshot`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `active` | `string` | 是 |  |
| `appearance` | `"dark" \| "light"` | 是 |  |
| `component` | `any` | 是 |  |
| `dark` | `string` | 是 |  |
| `icons` | `map&lt;string, string&gt;` | 是 |  |
| `light` | `string` | 是 |  |
| `preference` | `string` | 是 |  |
| `preview` | `string` |  |  |
| `revision` | `integer` | 是 |  |
| `styles` | `map&lt;string, object&gt;` | 是 |  |
| `syntax` | `string` |  |  |

#### `theme.get`

无参数。

结果: `mare.ThemeSnapshot`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `active` | `string` | 是 |  |
| `appearance` | `"dark" \| "light"` | 是 |  |
| `component` | `any` | 是 |  |
| `dark` | `string` | 是 |  |
| `icons` | `map&lt;string, string&gt;` | 是 |  |
| `light` | `string` | 是 |  |
| `preference` | `string` | 是 |  |
| `preview` | `string` |  |  |
| `revision` | `integer` | 是 |  |
| `styles` | `map&lt;string, object&gt;` | 是 |  |
| `syntax` | `string` |  |  |

#### `theme.list`

无参数。

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `{ label: string, name: string, origin: string, variants: "dark" \| "light"[] }[]` | 是 |  |

#### `theme.preview`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `theme` | `string` | 是 |  |

结果: `mare.ThemeSnapshot`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `active` | `string` | 是 |  |
| `appearance` | `"dark" \| "light"` | 是 |  |
| `component` | `any` | 是 |  |
| `dark` | `string` | 是 |  |
| `icons` | `map&lt;string, string&gt;` | 是 |  |
| `light` | `string` | 是 |  |
| `preference` | `string` | 是 |  |
| `preview` | `string` |  |  |
| `revision` | `integer` | 是 |  |
| `styles` | `map&lt;string, object&gt;` | 是 |  |
| `syntax` | `string` |  |  |

#### `theme.register`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `definition` | `object` | 是 |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `registration_id` | `string` | 是 |  |

#### `theme.register_override`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `layer_id` | `string` | 是 |  |
| `styles` | `map&lt;string, object&gt;` | 是 |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `registration_id` | `string` | 是 |  |

#### `theme.select`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `save` | `boolean` |  |  |
| `theme` | `string` | 是 |  |

结果: `mare.ThemeSnapshot`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `active` | `string` | 是 |  |
| `appearance` | `"dark" \| "light"` | 是 |  |
| `component` | `any` | 是 |  |
| `dark` | `string` | 是 |  |
| `icons` | `map&lt;string, string&gt;` | 是 |  |
| `light` | `string` | 是 |  |
| `preference` | `string` | 是 |  |
| `preview` | `string` |  |  |
| `revision` | `integer` | 是 |  |
| `styles` | `map&lt;string, object&gt;` | 是 |  |
| `syntax` | `string` |  |  |

### tools

#### `tools.register`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `access` | `{ kind: "file", op: "read" \| "write" \| "create" \| "delete", path_arg: string, recursive: boolean }[]` |  |  |
| `concurrency_safe` | `boolean` |  |  |
| `description` | `string` | 是 |  |
| `exposure` | `"direct" \| "deferred" \| "nested"` |  |  |
| `guidelines` | `string[]` |  |  |
| `idempotent` | `boolean` |  |  |
| `name` | `string` | 是 |  |
| `parameters` | `object` | 是 |  |
| `side_effect` | `"read" \| "write" \| "external"` |  |  |
| `snippet` | `string` |  |  |
| `timeout_ms` | `integer` |  |  |
| `callbacks` | `"execute"[]` | 是 |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `registration_id` | `string` | 是 |  |

### ui

#### `ui.attach`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |
| `responder` | `boolean \| null` |  |  |
| `takeover` | `string[]` |  | (default `[]`) |
| `viewport` | `UiViewport \| null` |  |  |

结果: `UiAttachResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |
| `session_epoch` | `string` | 是 |  |
| `responder` | `boolean` | 是 |  |
| `takeover` | `string[]` | 是 |  |
| `snapshot` | `UiShellSnapshot` | 是 |  |

#### `ui.call_keymap`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `action` | `string` |  |  |
| `context` | `string` |  |  |
| `key` | `string` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `action` | `string` |  |  |
| `context` | `string` |  |  |
| `declined` | `boolean` | 是 |  |
| `handled` | `boolean` | 是 |  |
| `masked` | `boolean` | 是 |  |
| `reason` | `string` |  |  |

#### `ui.cancel_markdown`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `document_id` | `string` | 是 |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `object`

#### `ui.delete_keymap`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `context` | `string` | 是 |  |
| `key` | `string` | 是 |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `context` | `string` | 是 |  |
| `key` | `string` | 是 |  |

#### `ui.delete_markdown`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `document_id` | `string` | 是 |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `object`

#### `ui.detach`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |

结果: `UiDetachResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |

#### `ui.get_keymap`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `context` | `string` |  | Optional context name to filter by. |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `actions` | `{ contexts?: string[], core?: boolean, desc?: string, essential?: boolean, keys?: string[], name: string }[]` | 是 |  |
| `bindings` | `object[]` | 是 |  |
| `conflicts` | `{ missing_essential: { action: string, context: string }[], unbound_essential: { action: string, context: string }[] }` | 是 |  |
| `contexts` | `{ bindable: boolean, chain: string[], core: boolean, name: string, parents: string[] }[]` | 是 |  |

#### `ui.get_markdown`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `document_id` | `string` | 是 |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `mare.MarkdownSnapshot`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `diagnostic` | `string` |  |  |
| `document_id` | `string` | 是 |  |
| `enhanced` | `boolean` | 是 |  |
| `expanded_node` | `mare.Node` | 是 |  |
| `node` | `mare.Node` | 是 |  |
| `pending` | `integer` | 是 |  |
| `render_seq` | `integer` | 是 |  |
| `revision` | `integer` | 是 |  |
| `session_epoch` | `string` |  |  |
| `session_id` | `string` |  |  |
| `source` | `string` | 是 |  |
| `supports_artifact_images` | `boolean` | 是 |  |

#### `ui.get_select_preview`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `index` | `integer` | 是 |  |
| `select_id` | `string` | 是 |  |
| `tab` | `integer` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `preview` | `string \| mare.Node` |  |  |

#### `ui.invalidate`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `component` | `string` |  | Widget (`widget:<id>` or widget id) or status segment id to redraw. |
| `viewport` | `{ height?: integer, supports_artifact_images?: boolean, supports_images?: boolean, width?: integer }` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `object`

#### `ui.invalidate_presentations`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `object`

#### `ui.list_markdown_presenters`

无参数。

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `{ language: string, preview: boolean, requires_artifact_images: boolean, timeout_ms: integer }[]` | 是 |  |

#### `ui.register_entry_renderer`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `kind` | `string` | 是 |  |
| `callbacks` | `"render"[]` | 是 |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `registration_id` | `string` | 是 |  |

#### `ui.register_markdown_transform`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `message_types` | `"user" \| "assistant" \| "assistant-reasoning" \| "custom" \| "summary" \| "changelog"[]` |  |  |
| `callbacks` | `"transform"[]` | 是 |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `registration_id` | `string` | 是 |  |

#### `ui.render_markdown`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `base_node` | `mare.Node` |  |  |
| `document_id` | `string` | 是 |  |
| `replay` | `boolean` |  |  |
| `source` | `string` | 是 |  |
| `viewport` | `{ height?: integer, supports_artifact_images?: boolean, supports_images?: boolean, width?: integer }` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果: `mare.MarkdownSnapshot`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `diagnostic` | `string` |  |  |
| `document_id` | `string` | 是 |  |
| `enhanced` | `boolean` | 是 |  |
| `expanded_node` | `mare.Node` | 是 |  |
| `node` | `mare.Node` | 是 |  |
| `pending` | `integer` | 是 |  |
| `render_seq` | `integer` | 是 |  |
| `revision` | `integer` | 是 |  |
| `session_epoch` | `string` |  |  |
| `session_id` | `string` |  |  |
| `source` | `string` | 是 |  |
| `supports_artifact_images` | `boolean` | 是 |  |

#### `ui.send_editor`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `attachments` | `object[]` |  |  |
| `mode` | `"prompt" \| "steer" \| "follow_up"` |  |  |
| `text` | `string` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `handled` | `boolean` | 是 |  |
| `result` | `object` |  |  |
| `revision` | `integer` | 是 |  |

#### `ui.set_external_editor`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `action` | `"start" \| "commit" \| "cancel"` | 是 |  |
| `text` | `string` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `program` | `string` |  |  |
| `revision` | `integer` | 是 |  |
| `running` | `boolean` | 是 |  |
| `text` | `string` | 是 |  |

#### `ui.set_keymap`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `action` | `string \| boolean` | 是 |  |
| `context` | `string` | 是 |  |
| `desc` | `string` |  |  |
| `key` | `string` | 是 |  |
| `priority` | `number` |  |  |
| `unique` | `boolean` |  |  |
| `when` | `string[]` |  |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `registration_id` | `string` | 是 |  |

#### `ui.transform_markdown`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `available_width` | `integer` |  |  |
| `created_at_ms` | `integer` |  |  |
| `is_streaming` | `boolean` |  |  |
| `message_id` | `string` |  |  |
| `message_type` | `"user" \| "assistant" \| "assistant-reasoning" \| "custom" \| "summary" \| "changelog"` |  |  |
| `text` | `string` | 是 |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `text` | `string` | 是 |  |

#### `ui.update_editor`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `attachments` | `{ attachment_id: string, height?: integer, label?: string, mime_type?: string, width?: integer }[]` |  |  |
| `image_markers` | `string[]` |  |  |
| `offset` | `integer` |  |  |
| `shell_mode` | `boolean` |  |  |
| `text` | `string` | 是 |  |
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `attachments` | `{ attachment_id: string, height?: integer, label?: string, mime_type?: string, width?: integer }[]` |  |  |
| `external_editor_running` | `boolean` | 是 |  |
| `image_markers` | `string[]` |  |  |
| `offset` | `integer` | 是 |  |
| `revision` | `integer` | 是 |  |
| `shell_mode` | `boolean` | 是 |  |
| `text` | `string` | 是 |  |

### usage

#### `usage.get`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |
| `expected_epoch` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `context_tokens` | `integer` | 是 |  |
| `rate_limits` | `map&lt;string, { credits?: string, limit_name?: string, primary?: object, reached?: string, received_at_ms: integer, secondary?: object }&gt;` | 是 |  |
| `totals` | `object` | 是 |  |

### web

#### `web.fetch`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `timeout_ms` | `integer` |  |  |
| `url` | `string` | 是 |  |

结果: `mare.WebFetchResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `content` | `string` | 是 |  |
| `content_type` | `string` |  |  |
| `requested_url` | `string` | 是 |  |
| `status` | `integer` |  |  |
| `truncated` | `boolean` |  |  |
| `url` | `string` |  |  |

#### `web.list_providers`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `kind` | `"search" \| "fetch"` | 是 |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `{ available: boolean, provider_id: string, selected: boolean, unconfined: boolean }[]` | 是 |  |
| `reason` | `string` |  |  |

#### `web.register_fetch`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `provider_id` | `string` | 是 |  |
| `unconfined` | `boolean` |  |  |
| `callbacks` | `"fetch"[]` | 是 |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `registration_id` | `string` | 是 |  |

#### `web.register_search`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `provider_id` | `string` | 是 |  |
| `unconfined` | `boolean` |  |  |
| `callbacks` | `"search"[]` | 是 |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `registration_id` | `string` | 是 |  |

#### `web.search`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `queries` | `string[]` | 是 |  |
| `timeout_ms` | `integer` |  |  |

结果: `mare.WebSearchResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `content` | `string` |  |  |
| `data` | `{ published_at_ms?: integer, snippet?: string, summary?: string, title?: string, url: string }[]` | 是 |  |
| `truncated` | `boolean` |  |  |

### webserver

#### `webserver.get`

无参数。

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `host` | `string` |  |  |
| `port` | `integer` |  |  |
| `running` | `boolean` | 是 |  |
| `tls` | `boolean` |  |  |
| `trusted_hosts` | `string[]` |  |  |
| `url` | `string` |  |  |
| `ws_url` | `string` |  |  |

#### `webserver.list_assets`

无参数。

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `{ asset_id: string, dir: string, fallback?: string, index?: string, plugin?: string, prefix: string }[]` | 是 |  |

#### `webserver.list_routes`

无参数。

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `data` | `{ method: string, owner_session_id?: string, path: string, plugin?: string, route_id: string }[]` | 是 |  |

#### `webserver.register_assets`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `dir` | `string` | 是 |  |
| `fallback` | `string` |  |  |
| `index` | `string` |  |  |
| `prefix` | `string` |  |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `registration_id` | `string` | 是 |  |

#### `webserver.register_route`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `method` | `"GET" \| "POST" \| "PUT" \| "PATCH" \| "DELETE" \| "HEAD" \| "ANY"` |  |  |
| `path` | `string` | 是 |  |
| `callbacks` | `"handle"[]` | 是 |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `registration_id` | `string` | 是 |  |

### workspace

#### `workspace.get`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cwd` | `string` | 是 |  |

结果:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `dirs` | `string[]` | 是 |  |
| `root` | `string` | 是 |  |
| `trusted` | `boolean` | 是 |  |

#### `workspace.set_trust`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `save` | `boolean` |  |  |
| `trusted` | `boolean` | 是 |  |
| `cwd` | `string` | 是 |  |

结果: `object`

## 通知（mare → 客户端）

### `$/cancel_request`

Cancels an outstanding mare-to-host request. Late replies to this ID are ignored.

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `id` | `RequestId` | 是 |  |

### `dropped`

Drop count for unretained `lossy` or refused events omitted from external stream consumers.

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `session_epoch` | `string \| null` |  |  |
| `count` | `integer ≥ 0` | 是 |  |

### `event`

Envelope of an `event` notification: unified across RPC and headless streams.

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `type` | `string` | 是 | kernel event name, e.g. `turn/started`, `item/delta`, `item/completed`, `turn/completed` |
| `session_id` | `string \| null` |  | session the event belongs to |
| `session_epoch` | `string \| null` |  | opaque in-memory epoch identifier that rotates on every session create, load, or switch |
| `seq` | `integer ≥ 0` |  | per-session, per-epoch sequence number (1-based, contiguous) (default `0`) |
| `turn_id` | `string \| null` |  | run (prompt) the event belongs to — matches `agent.prompt` `turn_id` |
| `ignorable` | `boolean \| null` |  | whether a client that does not recognize this event type may skip it |
| `data` | `any` |  |  |

### `fs/changed`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `watch_id` | `string` | 是 |  |
| `changed_paths` | `string[]` | 是 |  |
| `truncated` | `boolean` |  |  |

### `lagged`

Sequence range omitted when an ordinary consumer cursor fell behind the bounded event log.

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `session_epoch` | `string \| null` |  |  |
| `from_seq` | `integer ≥ 0` | 是 |  |
| `to_seq` | `integer ≥ 0` | 是 |  |

### `peer.message_status`

`peer.message_status` notification (mare → peer): the receiving user's decision on a held
message.

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `message_id` | `string` | 是 |  |
| `status` | `string` | 是 | "delivered" \| "rejected" |
| `reason` | `string \| null` |  |  |

### `snapshot`

`snapshot` notification (mare → host): emitted immediately after an `events.subscribe` reply
with `mode = "baseline"`, and immediately after `lagged` when a live cursor is evicted.

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `session_epoch` | `string` | 是 |  |
| `seq` | `integer ≥ 0` | 是 | Ring tail sequence number `S` at the moment of the non-yielding cut; following events start at `S + 1`. |
| `baseline_id` | `string \| null` |  | Opaque handle for paging `parts` via `baseline.read`. |
| `retained` | `object` |  | Retained state (`events`, `status`, `queue`, `active_turn`, `active_items` skeletons, pending approvals/interactions). (default `{}`) |
| `parts` | `string[]` |  | Baseline parts available to page via `baseline.read` (e.g. `["entries", "active_items"]`). (default `[]`) |

### `stream.failed`

`event` notification params — exactly one `mare -p --json` line.
An event stream is incomplete and this connection will close.

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `error` | `string` | 是 |  |

### `ui.editor_config`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `prompt` | `string \| null` |  |  |
| `placeholder` | `string \| null` |  |  |
| `busy_placeholder` | `string \| null` |  |  |
| `border` | `string \| null` |  |  |

### `ui.editor_hint`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `text` | `string` | 是 |  |

### `ui.editor_state`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `text` | `string` | 是 |  |
| `cursor` | `integer ≥ 0` | 是 |  |
| `shell_mode` | `boolean` |  | (default `false`) |
| `image_markers` | `string[]` |  |  |
| `attachments` | `UiEditorAttachment[]` |  |  |
| `external_editor_running` | `boolean` |  | (default `false`) |
| `revision` | `integer ≥ 0` | 是 |  |

### `ui.markdown_changed`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `document_id` | `string` | 是 |  |
| `source` | `string` | 是 |  |
| `revision` | `integer ≥ 0` | 是 |  |
| `render_seq` | `integer ≥ 0` | 是 |  |
| `session_id` | `string \| null` |  |  |
| `session_epoch` | `string \| null` |  |  |
| `pending` | `integer ≥ 0` | 是 |  |
| `enhanced` | `boolean` | 是 |  |
| `supports_artifact_images` | `boolean` | 是 |  |
| `diagnostic` | `string \| null` |  |  |
| `node` | `any` | 是 | Existing Node description. Image bytes become bounded canonical base64 `data`; no paths, callbacks, tokens, or userdata. Deserialize via mare.ui.node_from_wire in a Lua frontend. |
| `expanded_node` | `any` | 是 | Same Markdown document with each enhanced fence followed by its complete source. |

### `ui.notify`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `text` | `string` | 是 |  |
| `level` | `string \| null` |  | info \| success \| warning \| error |

### `ui.overlay`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `title` | `string` | 是 |  |
| `node` | `any` |  |  |
| `open` | `boolean` | 是 |  |
| `context` | `string \| null` |  |  |

### `ui.set_editor`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `text` | `string` | 是 |  |
| `keep_draft` | `boolean` |  | (default `false`) |
| `prepend` | `boolean` |  | (default `false`) |

### `ui.status`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `status_id` | `string` | 是 |  |
| `align` | `string` | 是 |  |
| `order` | `integer` |  | (default `0`) |
| `keep` | `boolean` |  | (default `false`) |
| `node` | `any` |  |  |

### `ui.widget`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `widget_id` | `string` | 是 |  |
| `slot` | `string` | 是 |  |
| `order` | `integer` |  | (default `0`) |
| `node` | `any` |  |  |

### `ui/component-updated`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 |  |
| `component` | `string` | 是 |  |
| `payload` | `any` |  |  |

### `ui/select-closed`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `select_id` | `string` | 是 |  |

### `ui/select-updated`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `select_id` | `string` | 是 |  |
| `items` | `UiSelectItem[]` |  |  |
| `tabs` | `UiSelectTab[]` |  |  |

## 通知（客户端 → mare）

### `$/cancel_request`

Cancels an outstanding mare-to-host request. Late replies to this ID are ignored.

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `id` | `RequestId` | 是 |  |

### `callback.update`

`callback.update`: client → server notification streaming a partial update for an in-flight
`callback.call` request identified by `id`.

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `id` | `RequestId` | 是 |  |
| `partial` | `any` | 是 |  |

## 回调（mare → 客户端的请求）

### `approval.request`

`approval.request` (mare → host): a tool call or a network host needs approval.
No answer (timeout, disconnect, error, invalid result) counts as `deny`.

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 | session that needs approval |
| `turn_id` | `string \| null` |  | active turn id, when known |
| `kind` | `string` | 是 | "tool" or "network" |
| `approval_id` | `string \| null` |  | unique approval lifecycle id (pairs with `approval/requested` and `approval/resolved`) |
| `tool_call_id` | `string \| null` |  | tool call id (kind = tool) |
| `tool` | `string \| null` |  | tool name (kind = tool) |
| `arguments` | `any` |  | the call's arguments (kind = tool) |
| `command` | `string \| null` |  | shell command, when the tool runs one |
| `diff` | `string \| null` |  | proposed change (patch / edit), when the tool writes files |
| `host` | `string \| null` |  | host:port (kind = network) |
| `cwd` | `string` | 是 |  |
| `reason` | `string \| null` |  | why approval is needed (from the policy vote) |
| `side_effect` | `string \| null` |  | "read" \| "write" \| "external" |
| `unconfined` | `boolean` |  | runs outside the local sandbox (MCP / RPC-host tools) (default `false`) |
| `proposed_amendment` | `any` |  | a rule the policy proposes for `allow_amend` (opaque to core) |
| `requested_permissions` | `PermissionProfile \| null` |  | Explicit permission increment requested by this tool call (read/write paths, network hosts). |
| `scope` | `PermissionGrantScope \| null` |  | Requested maximum lifetime, when this is a structured permission request. |

结果: `ApprovalRequestResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `decision` | `ApprovalDecision` | 是 |  |
| `permissions` | `PermissionProfile` |  | Optional structured subset of the requested read/write/network increment. |
| `scope` | `PermissionGrantScope` |  | Lifetime of the grant: turn or session (the request remains an upper bound). |
| `amendment` | `any` |  | with `allow_amend`: the rule to persist (opaque to core) |
| `reason` | `string \| null` |  | shown to the model when denied |

### `callback.call`

`callback.call`: server → client request invoking one contract callback on a registration the
client holds. Arguments match the callback's params schema; result is
checked against its result schema.

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `registration_id` | `string` | 是 |  |
| `callback` | `string` | 是 |  |
| `session_id` | `string \| null` |  |  |
| `turn_id` | `string \| null` |  |  |
| `tool_call_id` | `string \| null` |  |  |
| `caller_id` | `string \| null` |  |  |
| `arguments` | `any` | 是 |  |

结果: `any`

### `mcp.elicitation`

`mcp.elicitation` (mare → host): an MCP server requested user input or URL confirmation.

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string` | 是 | session that requested elicitation |
| `turn_id` | `string \| null` |  | active turn id, when known |
| `server` | `string` | 是 |  |
| `request_id` | `any` |  |  |
| `mode` | `string \| null` |  |  |
| `message` | `string` |  | (default `""`) |
| `requested_schema` | `any` |  |  |
| `url` | `string \| null` |  |  |
| `elicitation_id` | `string \| null` |  |  |
| `_meta` | `any` |  |  |

结果: `McpElicitationResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `action` | `McpElicitationAction` | 是 |  |
| `content` | `any` |  |  |

### `ui.confirm`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `prompt` | `string` | 是 |  |
| `default` | `boolean \| null` |  |  |

结果: `UiConfirmResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `confirmed` | `boolean \| null` |  | true/false when answered; null when cancelled or interrupted |
| `reason` | `string \| null` |  | Dismissal reason when cancelled (`"cancelled"` \| `"interrupted"`) |

### `ui.editor`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `title` | `string` |  | (default `""`) |
| `prefill` | `string` |  | (default `""`) |

结果: `UiTextPromptResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `text` | `string \| null` |  | null when cancelled |
| `reason` | `string \| null` |  | Dismissal reason when cancelled (`"cancelled"` \| `"interrupted"`) |

### `ui.input`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `title` | `string` |  | (default `""`) |
| `placeholder` | `string` |  | (default `""`) |
| `default` | `string \| null` |  |  |
| `secret` | `boolean` |  | (default `false`) |

结果: `UiTextPromptResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `text` | `string \| null` |  | null when cancelled |
| `reason` | `string \| null` |  | Dismissal reason when cancelled (`"cancelled"` \| `"interrupted"`) |

### `ui.questions`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `title` | `string \| null` |  |  |
| `questions` | `UiQuestionItem[]` | 是 |  |

结果: `UiQuestionsResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `answers` | `map&lt;string, UiQuestionAnswer&gt;` |  | (default `{}`) |
| `dismissed` | `boolean \| null` |  |  |
| `reason` | `string \| null` |  |  |
| `note` | `string \| null` |  |  |

### `ui.select`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `select_id` | `string \| null` |  |  |
| `title` | `string` |  | (default `""`) |
| `title_right` | `any` |  |  |
| `body` | `any[]` |  | Explanatory lines shown above the options (strings or wire `Node` objects). |
| `items` | `UiSelectItem[]` | 是 |  |
| `selected` | `integer ≥ 0` |  | 0-based preselected index (default `0`) |
| `multi` | `boolean` |  | Allow checking multiple items before confirming. (default `false`) |
| `accept_label` | `string \| null` |  |  |
| `allow_search` | `boolean \| null` |  | Defaults to true when omitted. |
| `filter` | `string \| null` |  |  |
| `tabs` | `UiSelectTab[]` |  |  |
| `tab` | `integer ≥ 0` |  | 0-based initial tab index (default `0`) |
| `kind` | `string \| null` |  | "list" \| "confirm" |
| `keys` | `UiSelectKey[]` |  |  |
| `vim` | `boolean \| null` |  |  |
| `has_highlight` | `boolean \| null` |  |  |

结果: `UiSelectResult`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `index` | `integer \| null` |  |  |
| `selected_indices` | `integer ≥ 0[] \| null` |  | 0-based checked item indices for a multi-select; an empty array is a valid answer. |
| `tab` | `integer \| null` |  |  |
| `key` | `string \| null` |  |  |
| `filter` | `string \| null` |  |  |
| `reason` | `string \| null` |  | Dismissal reason when cancelled (`"cancelled"` \| `"interrupted"`) |

## 错误码

错误的 `data.code` 说明出了什么错，`data.data` 是附加数据。服务自己的错误码写作 `<服务>.<名字>`，由服务契约声明（`services.list` 会列出）；其余一律为 `internal`。

| `data.code` | 编号 | 含义 |
|---|---|---|
| `parse_error` | `-32700` | the message is not valid JSON |
| `invalid_request` | `-32600` | the message is not a valid request |
| `method_not_found` | `-32601` | no such method, or it is not exported to this caller |
| `invalid_arguments` | `-32602` | the arguments do not match the method's schema |
| `already_provided` | `-32000` | the service already has a provider |
| `invalid_result` | `-32000` | the provider's result does not match the method's schema |
| `internal` | `-32603` | an unexpected failure; the message is bounded and carries no raw error value |
| `cancelled` | `-32800` | cancelled by the caller, an owner release or a shutdown |
| `inactive_ctx` | `-32000` | the caller's context was disposed; it can no longer register anything |
| `timed_out` | `-32000` | a deadline passed before the work finished |
| `disconnected` | `-32000` | the connection that implements or awaits the call went away |
| `denied` | `-32000` | not allowed for the connection this work runs for |
| `unavailable` | `-32000` | the service or its provider is not available |
| `unknown_session` | `-32000` | no session with that id |
| `stale_session` | `-32000` | the session changed since the id was obtained |
| `overloaded` | `-32000` | admission refused before the work started; try again later |
| `busy` | `-32000` | the target cannot take the request now and it cannot be queued |
| `unsupported` | `-32000` | no backend or model supports the requested capability |
| `unsupported_version` | `-32000` | the protocol major version is not supported |
| `request_id_conflict` | `-32000` | a request id was reused with different content |
| `persist_failed` | `-32000` | durable session persistence failed |
| `invalid_namespace` | `-32000` | the state namespace name is not valid |
| `invalid_value` | `-32000` | the state value is not valid |
| `read_only` | `-32000` | the state namespace is read-only here |
| `lock_timeout` | `-32000` | the state lock was not acquired in time |
| `corrupt` | `-32000` | the stored state cannot be read |
| `revision_exhausted` | `-32000` | the state revision counter is exhausted |
| `io` | `-32000` | the state could not be read or written |
| `unknown_state` | `-32000` | the state outcome is unknown |
| `baseline.stale` | `-32000` | the baseline handle is unknown, expired, superseded, completed, or from another session epoch |
| `baseline.unknown_part` | `-32000` | the requested baseline part is not valid for this baseline |
| `baseline.item_too_large` | `-32000` | a single baseline item exceeds the page byte budget |
