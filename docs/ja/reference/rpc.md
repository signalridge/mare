# RPC リファレンス

`mare --rpc` のすべてのメソッド、通知、コールバック。`mare rpc schema` から抽出しています。プロトコル全体のフィールド名は `snake_case` に統一されています。RPC でのセッションの流れと例は [JSON-RPC](../integrate/rpc.md) を参照してください。

> このページは `scripts/gen_docs.py` がコードから生成します。手で編集しないでください。説明文はコード由来のため英語のままです。

プロトコルバージョン: **3.0**

## メソッド（クライアント → mare）

### agent

#### `agent.cancel`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `clear_queue` | `boolean` |  |  |
| `mode` | `"cancel" \| "halt"` |  |  |
| `reason` | `string` |  | Cancellation cause (`user`, `parent`, `hook`, `disposed`) or free-form note (classified as `user`). |
| `request_id` | `string` |  |  |
| `turn_id` | `string` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `duplicate` | `boolean` |  |  |
| `removed` | `{ content?: object[], images: { data?: string, mime: string, path?: string, size?: integer }[], input_context?: any, input_id: string, lane: "steer" \| "follow_up", text: string }[]` | はい |  |
| `turn_id` | `string` |  |  |

#### `agent.continue`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `effort` | `string` |  |  |
| `tools` | `string \| object[]` |  |  |
| `output_schema` | `object` |  |  |
| `source` | `string` |  |  |
| `system` | `string` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `turn_id` | `string` | はい |  |

#### `agent.delete_queued`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `input_id` | `string` | はい |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `object`

#### `agent.get`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `activity` | `{ extra: object, extra_errors: object }` |  |  |
| `busy` | `boolean` | はい |  |
| `effort` | `string` |  |  |
| `interrupted` | `boolean` |  |  |
| `message_count` | `integer` |  |  |
| `model` | `string` |  |  |
| `queued` | `integer` | はい |  |
| `session_id` | `string` |  |  |
| `turn_cancelled` | `boolean` |  |  |
| `turn_id` | `string` |  |  |

#### `agent.list_messages`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `cursor` | `string` |  |  |
| `limit` | `integer` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `object[]` | はい |  |
| `next_cursor` | `string` |  |  |

#### `agent.list_queue`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `{ content?: object[], images: { data?: string, mime: string, path?: string, size?: integer }[], input_context?: any, input_id: string, lane: "steer" \| "follow_up", text: string }[]` | はい |  |

#### `agent.prompt`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `effort` | `string` |  |  |
| `when_busy` | `"queue" \| "reject"` |  |  |
| `input` | `string \| object[]` | はい |  |
| `output_schema` | `object` |  |  |
| `request_id` | `string` |  |  |
| `source` | `string` |  |  |
| `system` | `string` |  |  |
| `tools` | `string \| object[]` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `duplicate` | `boolean` |  |  |
| `input_id` | `string` |  |  |
| `status` | `"started" \| "queued"` | はい |  |
| `turn_id` | `string` |  |  |

#### `agent.steer`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `expected_turn_id` | `string` |  |  |
| `input` | `string \| object[]` | はい |  |
| `request_id` | `string` |  |  |
| `source` | `string` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `duplicate` | `boolean` |  |  |
| `input_id` | `string` | はい |  |

#### `agent.update_queued`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `input` | `string \| object[]` |  |  |
| `input_id` | `string` | はい |  |
| `lane` | `"steer" \| "follow_up"` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `content` | `object[]` |  | Optional ordered content blocks with image bytes stripped (preserved on cancel). |
| `images` | `{ data?: string, mime: string, path?: string, size?: integer }[]` | はい | Queued image attachments. |
| `input_context` | `any` |  | Opaque caller-supplied JSON metadata preserved with the queued input. |
| `input_id` | `string` | はい | Stable queued input id. |
| `lane` | `"steer" \| "follow_up"` | はい | Delivery lane (`steer` or `follow_up`). |
| `text` | `string` | はい | Queued input text. |

#### `agent.wait`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `timeout_ms` | `integer` |  |  |
| `turn_id` | `string` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `cancellation_cause` | `"user" \| "parent" \| "hook" \| "disposed"` |  | Origin of cancellation when `termination_reason` is `interrupted`. |
| `completed_at_ms` | `integer` |  | Unix epoch milliseconds when the turn finished. |
| `error` | `{ code: string, data?: any, message: string }` |  |  |
| `error_kind` | `string` |  | Classified failure kind when present. |
| `messages` | `object[]` |  | Session messages visible at turn end. |
| `not_sent` | `boolean` |  | True when the prompt failed before reaching the provider. |
| `save_warning` | `string` |  | Quarantine/save failure warning when another error took precedence. |
| `started_at_ms` | `integer` | はい | Unix epoch milliseconds when the turn started. |
| `status` | `"completed" \| "cancelled" \| "failed" \| "running"` | はい | Turn completion outcome (or `running` when `wait` timed out). |
| `structured` | `any` |  | Validated structured output when `output_schema` was supplied. |
| `termination_reason` | `"completed" \| "usage_limit" \| "interrupted" \| "aborted" \| "turn_limit" \| "error"` |  | Machine-readable turn termination outcome. |
| `text` | `string` |  | Final assistant text produced during the turn. |
| `timed_out` | `boolean` |  | True when `agent.wait` returned on `timeout_ms` before settlement. |
| `turn_id` | `string` | はい | Turn identifier. |
| `usage` | `object` |  | Aggregated token/cost usage for the turn. |

### baseline

#### `baseline.read`

Parameters for `baseline.read`.

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `baseline_id` | `string` | はい |  |
| `part` | `string` | はい |  |
| `cursor` | `string \| null` |  |  |
| `limit` | `integer \| null` |  | Optional maximum number of items to return in one page (&gt;= 1). |

結果: `BaselineReadResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `any[]` | はい |  |
| `next_cursor` | `string \| null` |  |  |

エラー: `baseline.stale`, `baseline.unknown_part`, `baseline.item_too_large`

### cells

#### `cells.cancel`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `cell_id` | `string` | はい |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `object`

#### `cells.get`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `retains_state` | `boolean` | はい |  |
| `running` | `integer` | はい |  |
| `store_durable` | `boolean` | はい |  |
| `stored_keys` | `integer` | はい |  |

#### `cells.list`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `cursor` | `string` |  |  |
| `limit` | `integer` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `{ cell_id: string, status: "running" \| "completed" \| "failed" \| "cancelled" }[]` | はい |  |
| `next_cursor` | `string` |  |  |

#### `cells.list_tools`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `cursor` | `string` |  |  |
| `limit` | `integer` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `{ declaration: string, description: string, name: string, output_schema: object, parameters: object }[]` | はい |  |
| `next_cursor` | `string` |  |  |

#### `cells.reset`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `object`

#### `cells.start`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `capabilities` | `string[]` |  |  |
| `cell_id` | `string` |  |  |
| `code` | `string` | はい |  |
| `max_output_tokens` | `integer` |  |  |
| `state_owner` | `string` |  |  |
| `timeout_ms` | `integer \| false` |  |  |
| `yield_time_ms` | `integer` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `mare.CellResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `cell_id` | `string` | はい |  |
| `content` | `any[]` | はい |  |
| `error` | `string` |  |  |
| `result` | `any` |  |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled"` | はい |  |
| `stop_unconfirmed` | `boolean` |  |  |

#### `cells.wait`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `cell_id` | `string` | はい |  |
| `max_output_tokens` | `integer` |  |  |
| `terminate` | `boolean` |  |  |
| `yield_time_ms` | `integer` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `mare.CellResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `cell_id` | `string` | はい |  |
| `content` | `any[]` | はい |  |
| `error` | `string` |  |  |
| `result` | `any` |  |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled"` | はい |  |
| `stop_unconfirmed` | `boolean` |  |  |

### commands

#### `commands.get`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `name` | `string` | はい |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `alias_of` | `string` |  |  |
| `args` | `string` |  |  |
| `auto_execute` | `boolean` |  |  |
| `availability` | `"always" \| "idle" \| "session" \| "main" \| "running" \| "tui" \| "always" \| "idle" \| "session" \| "main" \| "running" \| "tui"[]` |  |  |
| `description` | `string` |  |  |
| `local_only` | `boolean` |  |  |
| `name` | `string` | はい |  |

#### `commands.list`

パラメーターなし。

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `object[]` | はい |  |

#### `commands.register`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `alias_of` | `string` |  |  |
| `args` | `string` |  |  |
| `auto_execute` | `boolean` |  |  |
| `availability` | `"always" \| "idle" \| "session" \| "main" \| "running" \| "tui" \| "always" \| "idle" \| "session" \| "main" \| "running" \| "tui"[]` |  |  |
| `description` | `string` |  |  |
| `local_only` | `boolean` |  |  |
| `name` | `string` | はい |  |
| `replace` | `boolean` |  |  |
| `callbacks` | `"complete" \| "run"[]` |  |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `registration_id` | `string` | はい |  |

#### `commands.run`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `input` | `{ content?: object[] }` |  |  |
| `line` | `string` | はい |  |
| `session_id` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `result` | `any` |  |  |

### compact

#### `compact.run`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `instructions` | `string` |  |  |
| `messages` | `object[]` |  |  |
| `reason` | `"manual"` |  |  |
| `summarize_only` | `boolean` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `dropped` | `integer` |  |  |
| `status` | `"compacted" \| "nothing_to_compact" \| "unchanged" \| "summarized"` | はい |  |
| `summary` | `string` |  |  |
| `tokens_after` | `integer` | はい |  |
| `tokens_before` | `integer` | はい |  |

### config

#### `config.describe`

Return redacted config metadata, provenance, and redacted warnings.

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `keys` | `string[] \| null` |  | option names or dotted paths (default: all) |

結果: `ConfigDescribeResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `options` | `map&lt;string, OptionDescriptor&gt;` | はい |  |
| `warnings` | `string[]` |  |  |

### connection

#### `connection.set_capabilities`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `capabilities` | `StrictClientCapabilities` | はい |  |

結果: `SetCapabilitiesResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `capabilities` | `ClientCapabilities` | はい |  |

### events

#### `events.list`

パラメーターなし。

結果: `EventsListResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `EventInfo[]` | はい |  |

#### `events.subscribe`

`events.subscribe`: subscribe to (or filter) a session's event stream.

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string \| null` |  | session id to subscribe to (required unless `scope = "all"` is explicitly set by the owner) |
| `session_epoch` | `string \| null` |  | last observed `session_epoch`; mismatches fall back to `mode = "baseline"` |
| `last_seq` | `integer \| null` |  | last delivered `seq` within `session_epoch` for resumptive replay |
| `scope` | `RegistrationScopeKind \| null` |  |  |
| `include` | `string[] \| null` |  |  |
| `exclude` | `string[] \| null` |  |  |

結果: `EventsSubscribeResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `mode` | `SubscribeMode` | はい |  |
| `session_epoch` | `string` | はい |  |
| `seq` | `integer ≥ 0` | はい | sequence number at which the stream continues (`next_seq` for `live`/`baseline`, `last_seq + 1` for `replay`) |

### flags

#### `flags.get`

`flags.get` params.

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `name` | `string` | はい |  |

結果: `FlagsGetResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `name` | `string` | はい |  |
| `registered` | `boolean` | はい |  |
| `value` | `any` |  |  |

#### `flags.list`

パラメーターなし。

結果: `FlagsListResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `FlagEntry[]` | はい |  |

### fs

#### `fs.list`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `path` | `string` | はい |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `{ file_name: string, is_directory: boolean, is_file: boolean }[]` | はい |  |

#### `fs.read`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `max_bytes` | `integer` |  |  |
| `path` | `string` | はい |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data_base64` | `string` | はい |  |

#### `fs.stat`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `path` | `string` | はい |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `created_at_ms` | `integer` | はい |  |
| `is_directory` | `boolean` | はい |  |
| `is_file` | `boolean` | はい |  |
| `mode` | `integer` |  |  |
| `modified_at_ms` | `integer` | はい |  |
| `nlink` | `integer` |  |  |
| `size` | `integer` | はい |  |

#### `fs.watch`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `path` | `string` | はい |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `registration_id` | `string` | はい |  |

#### `fs.write`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `atomic` | `boolean` |  |  |
| `create_new` | `boolean` |  |  |
| `data_base64` | `string` | はい |  |
| `path` | `string` | はい |  |

結果: `object`

### hooks

#### `hooks.subscribe`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `event` | `string` | はい |  |
| `prepend` | `boolean` |  |  |
| `timeout_ms` | `integer` |  |  |
| `callbacks` | `"handle"[]` | はい |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `registration_id` | `string` | はい |  |

### `initialize`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `protocol_version` | `string` | はい | Protocol version the host speaks (`major.minor`); the major version must match. |
| `client` | `ClientInfo \| null` |  |  |
| `capabilities` | `ClientCapabilities` |  | Callbacks the host can answer. Undeclared callbacks are never sent (defaults apply). (default `{"callbacks": false, "ui": false, "approvals": false, "elicitation": false}`) |
| `callback_timeout_ms` | `integer \| null` |  | Timeout for callbacks into the host in milliseconds (default 30000, clamped to 1..=3600000). |
| `approval_timeout_ms` | `integer \| null` |  | Optional human approval timeout in milliseconds; absent means wait without a deadline. |

結果: `InitializeResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `protocol_version` | `string` | はい |  |
| `server` | `ServerInfo` | はい |  |
| `capabilities` | `ServerCapabilities` | はい |  |
| `session` | `any` |  | Live `mare.Session` snapshot of the opened initial session, if any (`session_id` and `session_epoch` live on this object; session-scoped contract methods take `session_id`). |

### jobs

#### `jobs.background`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `job_id` | `string` | はい |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `object`

#### `jobs.cancel`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `job_id` | `string` | はい |  |
| `reason` | `string` |  |  |
| `silent` | `boolean` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `object`

#### `jobs.list`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `cursor` | `string` |  |  |
| `limit` | `integer` |  |  |
| `owner` | `string` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `object[]` | はい |  |
| `next_cursor` | `string` |  |  |

#### `jobs.read`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `job_id` | `string` | はい |  |
| `max_bytes` | `integer` |  |  |
| `offset` | `integer` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `done` | `boolean` |  |  |
| `dropped` | `integer` |  |  |
| `next_offset` | `integer` |  |  |
| `text` | `string` | はい |  |

### llm

#### `llm.get_model`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `ref` | `string` |  | describe this model instead (hidden or unlisted ones too); the selection is unchanged |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `available` | `boolean` | はい |  |
| `context_window` | `integer` |  |  |
| `cost` | `{ cache_read?: number, cache_write?: number, input?: number, output?: number }` |  |  |
| `default_effort` | `string` |  |  |
| `edit_tool` | `"apply_patch" \| "replace"` |  |  |
| `efforts` | `string[]` | はい |  |
| `images` | `boolean` | はい |  |
| `max_output` | `integer` |  |  |
| `model_id` | `string` | はい |  |
| `name` | `string` |  |  |
| `parameters` | `object[]` | はい |  |
| `provider` | `string` | はい |  |
| `ready` | `boolean` | はい |  |
| `ready_reason` | `string` |  |  |
| `reasoning` | `boolean` | はい |  |
| `ref` | `string` | はい |  |
| `route` | `string` |  |  |
| `route_error` | `string` |  |  |
| `tool_update` | `"addition_only"` |  |  |
| `unavailable_reason` | `string` |  |  |
| `wire_route` | `string` |  |  |

#### `llm.list_auth_keys`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `prefix` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `string[]` | はい |  |

#### `llm.list_models`

パラメーターなし。

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `object[]` | はい |  |

#### `llm.list_providers`

パラメーターなし。

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `object[]` | はい |  |

#### `llm.login`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `key` | `string` |  |  |
| `method` | `"oauth" \| "device" \| "api_key" \| "auto"` |  |  |
| `provider` | `string` | はい |  |

結果: `object`

#### `llm.logout`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `provider` | `string` | はい |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `kept` | `string[]` | はい |  |
| `removed` | `string[]` | はい |  |
| `warnings` | `string[]` | はい |  |

#### `llm.register_provider`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `default_model` | `string` |  |  |
| `models` | `object[]` |  |  |
| `provider` | `string` | はい |  |
| `replace` | `boolean` |  |  |
| `callbacks` | `"stream"[]` |  |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `registration_id` | `string` | はい |  |

#### `llm.set_effort`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `also_global` | `boolean` |  |  |
| `effort` | `string` | はい |  |
| `save` | `boolean` |  |  |
| `scope_name` | `string` |  |  |
| `target` | `"session" \| "global"` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `object`

#### `llm.set_model`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `ref` | `string` | はい |  |
| `save` | `boolean` |  |  |
| `target` | `"session" \| "global"` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `available` | `boolean` | はい |  |
| `context_window` | `integer` |  |  |
| `cost` | `{ cache_read?: number, cache_write?: number, input?: number, output?: number }` |  |  |
| `default_effort` | `string` |  |  |
| `edit_tool` | `"apply_patch" \| "replace"` |  |  |
| `efforts` | `string[]` | はい |  |
| `images` | `boolean` | はい |  |
| `max_output` | `integer` |  |  |
| `model_id` | `string` | はい |  |
| `name` | `string` |  |  |
| `parameters` | `object[]` | はい |  |
| `provider` | `string` | はい |  |
| `ready` | `boolean` | はい |  |
| `ready_reason` | `string` |  |  |
| `reasoning` | `boolean` | はい |  |
| `ref` | `string` | はい |  |
| `route` | `string` |  |  |
| `route_error` | `string` |  |  |
| `tool_update` | `"addition_only"` |  |  |
| `unavailable_reason` | `string` |  |  |
| `wire_route` | `string` |  |  |

### mcp

#### `mcp.connect`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `server` | `string` | はい |  |

結果: `mare.McpServerStatus`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `error` | `string` |  |  |
| `missing` | `string[]` |  |  |
| `name` | `string` | はい |  |
| `source` | `string` |  |  |
| `state` | `string` | はい |  |
| `target` | `string` |  |  |
| `tier` | `string` |  |  |
| `tools` | `integer` | はい |  |
| `transport` | `string` |  |  |

#### `mcp.get_prompt`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `arguments` | `map&lt;string, string&gt;` |  |  |
| `name` | `string` | はい |  |
| `server` | `string` | はい |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `description` | `string` |  |  |
| `messages` | `object[]` | はい |  |

#### `mcp.list_prompts`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `connect` | `boolean` |  |  |
| `server` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `object[]` | はい |  |

#### `mcp.list_resources`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `connect` | `boolean` |  |  |
| `server` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `object[]` | はい |  |
| `resource_templates` | `object[]` | はい |  |

#### `mcp.list_servers`

パラメーターなし。

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `mare.McpServerStatus[]` | はい |  |
| `errors` | `string[]` | はい |  |

#### `mcp.list_tools`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `connect` | `boolean` |  |  |
| `server` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `object[]` | はい |  |

#### `mcp.read_resource`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `server` | `string` | はい |  |
| `uri` | `string` | はい |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `contents` | `object[]` | はい |  |

#### `mcp.reconnect`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `server` | `string` | はい |  |

結果: `mare.McpServerStatus`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `error` | `string` |  |  |
| `missing` | `string[]` |  |  |
| `name` | `string` | はい |  |
| `source` | `string` |  |  |
| `state` | `string` | はい |  |
| `target` | `string` |  |  |
| `tier` | `string` |  |  |
| `tools` | `integer` | はい |  |
| `transport` | `string` |  |  |

### message

#### `message.append`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `content` | `string \| any[]` | はい |  |
| `created_at_ms` | `integer` |  |  |
| `delivery` | `{ deliver_as?: "steer" \| "follow_up" \| "next_turn", trigger_turn?: boolean, wait?: boolean }` |  |  |
| `details` | `any` |  |  |
| `display` | `boolean` |  |  |
| `request_id` | `string` |  |  |
| `type` | `string` | はい |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `duplicate` | `boolean` |  |  |
| `message` | `object` | はい |  |
| `result` | `object` |  |  |
| `status` | `"appended" \| "queued" \| "deferred" \| "triggered"` | はい |  |

#### `message.list_types`

パラメーターなし。

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `{ type: string }[]` | はい |  |

#### `message.register_type`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `type` | `string` | はい |  |
| `callbacks` | `"render" \| "validate"[]` |  |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `registration_id` | `string` | はい |  |

### options

#### `options.get`

Read effective option values (matching `mare.o.<name>`). Sensitive fields (`config_sensitive`, `providers` keys/headers/query, credentials) are always redacted like `/config`.

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |
| `keys` | `string[] \| null` |  | option names or dotted paths (default: all) |
| `describe` | `boolean` |  | return per-key metadata (`effective`, `default`, `source`, `description`, `redacted`, …) instead of bare values (default `false`) |

結果: `object`

#### `options.set`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |
| `values` | `object` | はい | name → value (e.g. `{"model": "openai/gpt-6.1-terra", "effort": "high"}`) |

結果: `EmptyResult`

### peer

#### `peer.info`

パラメーターなし。

結果: `PeerInfo`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `name` | `string` | はい |  |
| `pid` | `integer ≥ 0` | はい |  |
| `cwd` | `string` | はい |  |
| `version` | `string` | はい | mare version |
| `allowed` | `string[]` | はい | methods this connection may call (default set + the session's `peer.allow`) |
| `delivery_modes` | `string[]` |  | supported inbound message delivery modes (e.g. `trigger_turn`, `queue_only`) |

#### `peer.message`

`peer.message` params: a message from another session. Where the socket reports the
connecting process (Unix credentials), a `from` that disagrees with it is refused: a `pid`
other than the connecting process's, or the `name` of a registered session that isn't it.
Everything else in `from`, and `policy`, is self-reported: shown only, never grounds for
allowing anything. Always held until the receiving user confirms; a plugin can
take it over via the `peer/inbound` event. Never an approval.

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `from` | `PeerRef` |  | (default `{"name": null, "pid": null, "cwd": null, "session_id": null}`) |
| `text` | `string` | はい |  |
| `queue_only` | `boolean` |  | enqueue the message without waking an idle receiver (default `false`) |
| `policy` | `any` |  | the sender's policy as it reports it (display only until identities can be verified) |

結果: `PeerMessageResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `message_id` | `string` | はい | id of this message (repeated in `peer.message_status`) |
| `status` | `string` | はい | "held" (waiting for the receiving user; the outcome follows as `peer.message_status`) or "handled" (a plugin took it over via `peer/inbound`) |

### permissions

#### `permissions.get`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `desc` | `string` |  |  |
| `grants` | `{ network?: boolean \| string[], read?: string[], write?: string[] }` |  |  |
| `label` | `string` | はい |  |
| `level` | `string` |  |  |
| `like` | `"read-only" \| "auto" \| "full-access"` |  |  |
| `partial` | `boolean` | はい |  |
| `policy_revision` | `integer` | はい |  |
| `preset` | `string` | はい |  |
| `rules` | `object[]` |  |  |

#### `permissions.list`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `{ current: boolean, desc: string, label: string, level: string, like: "read-only" \| "auto" \| "full-access", name: string }[]` | はい |  |

#### `permissions.set`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `confirm` | `boolean` |  |  |
| `name` | `string` | はい |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `desc` | `string` |  |  |
| `grants` | `{ network?: boolean \| string[], read?: string[], write?: string[] }` |  |  |
| `label` | `string` | はい |  |
| `level` | `string` |  |  |
| `like` | `"read-only" \| "auto" \| "full-access"` |  |  |
| `partial` | `boolean` | はい |  |
| `policy_revision` | `integer` | はい |  |
| `preset` | `string` | はい |  |
| `rules` | `object[]` |  |  |

### plugins

#### `plugins.check`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `names` | `string[]` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `object[]` | はい |  |

#### `plugins.install`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `name` | `string` |  | Optional plugin short name or declared source name; omitted operates on all eligible sources. |
| `reload` | `boolean` |  | When true and the package operation succeeds, run a full host reload. |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `log` | `string[]` | はい |  |
| `reload` | `{ added: string[], notices: string[], reloaded: string[], removed: string[], replaced: string[] }` |  |  |

#### `plugins.list`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `kind` | `"skills" \| "prompts" \| "themes" \| "types"` |  | Optional content kind (`skills \| prompts \| themes \| types`) for `dirs`; omitted returns package roots. |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `object[]` | はい |  |
| `diagnostics` | `{ constraint?: string, message: string, path: string, plugin: string }[]` | はい |  |
| `dirs` | `{ dir: string, name: string }[]` | はい |  |

#### `plugins.uninstall`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `name` | `string` |  | Optional plugin short name or declared source name; omitted operates on all eligible sources. |
| `reload` | `boolean` |  | When true and the package operation succeeds, run a full host reload. |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `log` | `string[]` | はい |  |
| `reload` | `{ added: string[], notices: string[], reloaded: string[], removed: string[], replaced: string[] }` |  |  |

#### `plugins.update`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `name` | `string` |  | Optional plugin short name or declared source name; omitted operates on all eligible sources. |
| `reload` | `boolean` |  | When true and the package operation succeeds, run a full host reload. |
| `stash` | `boolean` |  | Save local changes and untracked files in git stash before updating. |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `log` | `string[]` | はい |  |
| `reload` | `{ added: string[], notices: string[], reloaded: string[], removed: string[], replaced: string[] }` |  |  |

### query

#### `query.call`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `arguments` | `object` | はい |  |
| `operation` | `string` | はい |  |
| `provider` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `next_cursor` | `string` |  |  |
| `provider` | `string` |  |  |

#### `query.list_providers`

パラメーターなし。

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `{ name: string, operations: string[], priority: integer, revision: string, source: { capability: "in_process" \| "opaque" } }[]` | はい |  |

### registrations

#### `registrations.release`

Release one registration this connection holds; answered once it is withdrawn.

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `registration_id` | `string` | はい |  |

結果: `ReleaseResult`

### services

#### `services.list`

パラメーターなし。

結果: `ServicesListResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `ServiceInfo[]` | はい |  |

#### `services.register`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `name` | `string` | はい |  |
| `methods` | `map&lt;string, MethodSpec&gt;` | はい |  |
| `scope` | `RegistrationScopeKind \| null` |  |  |
| `session_id` | `string \| null` |  |  |

結果: `Registration`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `registration_id` | `string` | はい |  |
| `name` | `string` | はい |  |

### session

#### `session.add_record`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `any` |  |  |
| `kind` | `string` | はい |  |
| `persist` | `boolean` |  |  |
| `sync` | `boolean` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `durable` | `boolean` | はい |  |
| `record_id` | `string` | はい |  |

#### `session.archive`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |

結果: `object`

#### `session.close`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `expected_epoch` | `string` |  |  |
| `session_id` | `string` | はい |  |

結果: `object`

#### `session.create`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `cwd` | `string` |  |  |
| `name` | `string` |  |  |
| `request_id` | `string` |  |  |
| `subscribe` | `boolean` |  |  |

結果: `mare.LiveSession`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `archived` | `boolean` |  | Whether the session is archived in the index. |
| `created_at_ms` | `integer` | はい | Session creation timestamp in milliseconds. |
| `cwd` | `string` | はい | Working directory of the session. |
| `ephemeral` | `boolean` |  | Whether the session is running with persistence disabled. |
| `leaf_entry_id` | `string` |  | Current leaf entry id when loaded. |
| `loaded` | `boolean` | はい | Whether this session is loaded in the current host. |
| `name` | `string` |  | Optional user-assigned or generated session name. |
| `name_revision` | `integer` |  | Monotonic revision of the session name when loaded. |
| `parent_session_id` | `string` |  | Parent or fork-origin session id, if any. |
| `path` | `string` |  | On-disk JSONL file path when saved. |
| `session_epoch` | `string` | はい | In-memory session epoch when loaded. |
| `session_id` | `string` | はい | Unique session id. |
| `status` | `"idle" \| "busy"` |  | Live activity status when loaded. |
| `updated_at_ms` | `integer` | はい | Last update timestamp in milliseconds. |

#### `session.delete`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |

結果: `object`

#### `session.fork`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `before_item_id` | `string` |  |  |
| `expected_epoch` | `string` |  |  |
| `name` | `string` |  |  |
| `request_id` | `string` |  |  |
| `session_id` | `string` | はい |  |
| `subscribe` | `boolean` |  |  |

結果: `mare.LiveSession`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `archived` | `boolean` |  | Whether the session is archived in the index. |
| `created_at_ms` | `integer` | はい | Session creation timestamp in milliseconds. |
| `cwd` | `string` | はい | Working directory of the session. |
| `ephemeral` | `boolean` |  | Whether the session is running with persistence disabled. |
| `leaf_entry_id` | `string` |  | Current leaf entry id when loaded. |
| `loaded` | `boolean` | はい | Whether this session is loaded in the current host. |
| `name` | `string` |  | Optional user-assigned or generated session name. |
| `name_revision` | `integer` |  | Monotonic revision of the session name when loaded. |
| `parent_session_id` | `string` |  | Parent or fork-origin session id, if any. |
| `path` | `string` |  | On-disk JSONL file path when saved. |
| `session_epoch` | `string` | はい | In-memory session epoch when loaded. |
| `session_id` | `string` | はい | Unique session id. |
| `status` | `"idle" \| "busy"` |  | Live activity status when loaded. |
| `updated_at_ms` | `integer` | はい | Last update timestamp in milliseconds. |

#### `session.get`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |

結果: `mare.Session`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `archived` | `boolean` |  | Whether the session is archived in the index. |
| `created_at_ms` | `integer` | はい | Session creation timestamp in milliseconds. |
| `cwd` | `string` | はい | Working directory of the session. |
| `ephemeral` | `boolean` |  | Whether the session is running with persistence disabled. |
| `leaf_entry_id` | `string` |  | Current leaf entry id when loaded. |
| `loaded` | `boolean` | はい | Whether this session is loaded in the current host. |
| `name` | `string` |  | Optional user-assigned or generated session name. |
| `name_revision` | `integer` |  | Monotonic revision of the session name when loaded. |
| `parent_session_id` | `string` |  | Parent or fork-origin session id, if any. |
| `path` | `string` |  | On-disk JSONL file path when saved. |
| `session_epoch` | `string` |  | In-memory session epoch when loaded. |
| `session_id` | `string` | はい | Unique session id. |
| `source_leaf_entry_id` | `string` |  | Source leaf entry id observed when creating a snapshot, if any. |
| `status` | `"idle" \| "busy"` |  | Live activity status when loaded. |
| `updated_at_ms` | `integer` | はい | Last update timestamp in milliseconds. |

#### `session.get_item`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `after` | `integer` |  |  |
| `before` | `integer` |  |  |
| `item_id` | `string` | はい |  |
| `max_bytes` | `integer` |  |  |
| `offset` | `integer` |  |  |
| `session_id` | `string` | はい |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `chunk` | `string` |  |  |
| `chunk_offset` | `integer` |  |  |
| `corpus_revision` | `string` | はい |  |
| `item` | `object` |  |  |
| `item_id` | `string` | はい |  |
| `neighbors` | `{ item_id: string, type: string }[]` | はい |  |
| `next_offset` | `integer` |  |  |
| `seq` | `integer` | はい |  |
| `session_id` | `string` | はい |  |
| `total_bytes` | `integer` |  |  |

#### `session.label_item`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `item_id` | `string` |  |  |
| `label` | `string` | はい |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `item_id` | `string` | はい |  |
| `label` | `string` |  |  |

#### `session.list`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `all` | `boolean` |  |  |
| `archived` | `boolean` |  |  |
| `cursor` | `string` |  |  |
| `cwd` | `string` |  |  |
| `limit` | `integer` |  |  |
| `loaded` | `boolean` |  |  |
| `parent_session_id` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `mare.Session[]` | はい |  |
| `next_cursor` | `string` |  |  |

#### `session.list_items`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `branch` | `"current" \| "all"` |  |  |
| `cursor` | `string` |  |  |
| `direction` | `"asc" \| "desc"` |  |  |
| `limit` | `integer` |  |  |
| `types` | `string[]` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `object[]` | はい |  |
| `leaf_entry_id` | `string` |  |  |
| `next_cursor` | `string` |  |  |

#### `session.list_records`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `branch` | `"current" \| "all"` |  |  |
| `cursor` | `string` |  |  |
| `kind` | `string` | はい |  |
| `limit` | `integer` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `any[]` | はい |  |
| `next_cursor` | `string` |  |  |

#### `session.load`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `path` | `string` |  |  |
| `session_id` | `string` |  |  |
| `subscribe` | `boolean` |  |  |

結果: `mare.LiveSession`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `archived` | `boolean` |  | Whether the session is archived in the index. |
| `created_at_ms` | `integer` | はい | Session creation timestamp in milliseconds. |
| `cwd` | `string` | はい | Working directory of the session. |
| `ephemeral` | `boolean` |  | Whether the session is running with persistence disabled. |
| `leaf_entry_id` | `string` |  | Current leaf entry id when loaded. |
| `loaded` | `boolean` | はい | Whether this session is loaded in the current host. |
| `name` | `string` |  | Optional user-assigned or generated session name. |
| `name_revision` | `integer` |  | Monotonic revision of the session name when loaded. |
| `parent_session_id` | `string` |  | Parent or fork-origin session id, if any. |
| `path` | `string` |  | On-disk JSONL file path when saved. |
| `session_epoch` | `string` | はい | In-memory session epoch when loaded. |
| `session_id` | `string` | はい | Unique session id. |
| `status` | `"idle" \| "busy"` |  | Live activity status when loaded. |
| `updated_at_ms` | `integer` | はい | Last update timestamp in milliseconds. |

#### `session.rewind`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `before_item_id` | `string` |  |  |
| `expected_epoch` | `string` |  |  |
| `restore_files` | `boolean` |  |  |
| `session_id` | `string` | はい |  |
| `summarize` | `boolean` |  |  |

結果: `mare.LiveSession`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `archived` | `boolean` |  | Whether the session is archived in the index. |
| `created_at_ms` | `integer` | はい | Session creation timestamp in milliseconds. |
| `cwd` | `string` | はい | Working directory of the session. |
| `ephemeral` | `boolean` |  | Whether the session is running with persistence disabled. |
| `leaf_entry_id` | `string` |  | Current leaf entry id when loaded. |
| `loaded` | `boolean` | はい | Whether this session is loaded in the current host. |
| `name` | `string` |  | Optional user-assigned or generated session name. |
| `name_revision` | `integer` |  | Monotonic revision of the session name when loaded. |
| `parent_session_id` | `string` |  | Parent or fork-origin session id, if any. |
| `path` | `string` |  | On-disk JSONL file path when saved. |
| `session_epoch` | `string` | はい | In-memory session epoch when loaded. |
| `session_id` | `string` | はい | Unique session id. |
| `status` | `"idle" \| "busy"` |  | Live activity status when loaded. |
| `updated_at_ms` | `integer` | はい | Last update timestamp in milliseconds. |

#### `session.search`

| フィールド | 型 | 必須 | 説明 |
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

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `corpus_revision` | `string` | はい |  |
| `data` | `object[]` | はい |  |
| `incomplete` | `boolean` | はい |  |
| `next_cursor` | `string` |  |  |

#### `session.search_items`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `cursor` | `string` |  |  |
| `event_filters` | `{ event_types?: string[], from_ms?: integer, seq_from?: integer, seq_to?: integer, surfaces?: "current" \| "shadowed" \| "log_only"[], to_ms?: integer }` |  |  |
| `filters` | `object` |  |  |
| `limit` | `integer` |  |  |
| `query` | `string` |  |  |
| `session_id` | `string` | はい |  |
| `sort_direction` | `"asc" \| "desc"` |  |  |
| `sort_key` | `"relevance" \| "created_at_ms" \| "updated_at_ms"` |  |  |
| `step_bound` | `boolean` |  |  |
| `title_query` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `corpus_revision` | `string` | はい |  |
| `data` | `object[]` | はい |  |
| `incomplete` | `boolean` | はい |  |
| `next_cursor` | `string` |  |  |

#### `session.snapshot`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `before_item_id` | `string` |  |  |
| `effort` | `string` |  |  |
| `expected_epoch` | `string` |  |  |
| `initiating_tool_call_id` | `string` |  |  |
| `last_completed_turns` | `integer` |  |  |
| `model` | `string` |  |  |
| `name` | `string` |  |  |
| `request_id` | `string` |  |  |
| `session_id` | `string` | はい |  |

結果: `mare.Session`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `archived` | `boolean` |  | Whether the session is archived in the index. |
| `created_at_ms` | `integer` | はい | Session creation timestamp in milliseconds. |
| `cwd` | `string` | はい | Working directory of the session. |
| `ephemeral` | `boolean` |  | Whether the session is running with persistence disabled. |
| `leaf_entry_id` | `string` |  | Current leaf entry id when loaded. |
| `loaded` | `boolean` | はい | Whether this session is loaded in the current host. |
| `name` | `string` |  | Optional user-assigned or generated session name. |
| `name_revision` | `integer` |  | Monotonic revision of the session name when loaded. |
| `parent_session_id` | `string` |  | Parent or fork-origin session id, if any. |
| `path` | `string` |  | On-disk JSONL file path when saved. |
| `session_epoch` | `string` |  | In-memory session epoch when loaded. |
| `session_id` | `string` | はい | Unique session id. |
| `source_leaf_entry_id` | `string` |  | Source leaf entry id observed when creating a snapshot, if any. |
| `status` | `"idle" \| "busy"` |  | Live activity status when loaded. |
| `updated_at_ms` | `integer` | はい | Last update timestamp in milliseconds. |

#### `session.trace`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `ancestors` | `{ created_at_ms?: integer, session_id: string, title_available: boolean }[]` | はい |  |
| `complete` | `boolean` | はい |  |
| `descendants` | `object[]` | はい |  |
| `incomplete` | `boolean` | はい |  |
| `metadata_only` | `boolean` | はい |  |
| `parent_boundary` | `boolean` | はい |  |
| `root` | `{ created_at_ms?: integer, session_id: string, title_available: boolean }` |  |  |
| `scope` | `string` | はい |  |
| `target` | `{ created_at_ms?: integer, session_id: string, title_available: boolean }` | はい |  |

#### `session.trace_item`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `item_id` | `string` | はい |  |
| `session_id` | `string` | はい |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `availability` | `string` | はい |  |
| `created_at_ms` | `integer` |  |  |
| `derived_event_seqs` | `integer[]` | はい |  |
| `entry_id` | `string` |  |  |
| `incomplete` | `boolean` | はい |  |
| `item_id` | `string` | はい |  |
| `replaced_by` | `integer` |  |  |
| `replaced_event_seqs` | `integer[]` | はい |  |
| `replacement_chain` | `integer[]` | はい |  |
| `result_entry_id` | `string` |  |  |
| `role` | `string` |  |  |
| `seq` | `integer` | はい |  |
| `session_id` | `string` | はい |  |
| `source_event_seqs` | `integer[]` | はい |  |
| `summary` | `string` | はい |  |
| `surface` | `string` | はい |  |
| `type` | `string` | はい |  |

#### `session.unarchive`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |

結果: `object`

#### `session.update`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `expected_revision` | `integer` |  |  |
| `name` | `string` | はい |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `mare.LiveSession`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `archived` | `boolean` |  | Whether the session is archived in the index. |
| `created_at_ms` | `integer` | はい | Session creation timestamp in milliseconds. |
| `cwd` | `string` | はい | Working directory of the session. |
| `ephemeral` | `boolean` |  | Whether the session is running with persistence disabled. |
| `leaf_entry_id` | `string` |  | Current leaf entry id when loaded. |
| `loaded` | `boolean` | はい | Whether this session is loaded in the current host. |
| `name` | `string` |  | Optional user-assigned or generated session name. |
| `name_revision` | `integer` |  | Monotonic revision of the session name when loaded. |
| `parent_session_id` | `string` |  | Parent or fork-origin session id, if any. |
| `path` | `string` |  | On-disk JSONL file path when saved. |
| `session_epoch` | `string` | はい | In-memory session epoch when loaded. |
| `session_id` | `string` | はい | Unique session id. |
| `status` | `"idle" \| "busy"` |  | Live activity status when loaded. |
| `updated_at_ms` | `integer` | はい | Last update timestamp in milliseconds. |

### `shutdown`

パラメーターなし。

結果: `EmptyResult`

### spill

#### `spill.delete`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `spill_id` | `string` | はい |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `object`

#### `spill.list_backends`

パラメーターなし。

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `{ name: string, selected: boolean }[]` | はい |  |

#### `spill.read`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `encoding` | `"text" \| "base64"` |  |  |
| `limit` | `integer` |  |  |
| `offset` | `integer` |  |  |
| `spill_id` | `string` | はい |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `content` | `string` |  |  |
| `data_base64` | `string` |  |  |
| `next_offset` | `integer` |  |  |
| `size` | `integer` |  |  |

#### `spill.save`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `backend` | `string` |  |  |
| `content` | `string` |  |  |
| `data_base64` | `string` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `size` | `integer` | はい |  |
| `spill_id` | `string` | はい |  |

### state

#### `state.compare_exchange`

Atomically replace durable namespace state at `expected_revision` (`ok`, `conflict` with the current snapshot, or `rejected`).
Strict JSON preserves null and empty containers; deletion requires `null` and retains a monotonic revision. Cancellation before publication prevents writes; after publication admission rollback is not guaranteed, and `unknown_state` requires reread/reconciliation.

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `namespace` | `string` | はい |  |
| `expected_revision` | `integer ≥ 0` | はい |  |
| `value` | `any` | はい |  |
| `delete` | `boolean` |  | (default `false`) |

結果: `StateCompareExchangeResult`

#### `state.snapshot`

Read isolated durable application state (distinct from `agent.get`).
Absent or deleted namespaces report `exists: false` and `null` value with tombstone revision retained.

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `namespace` | `string` | はい |  |

結果: `StateSnapshotResult`

### status

#### `status.get`

パラメーターなし。

結果: `mare.Status`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `cwd` | `string` | はい |  |
| `message` | `string` | はい |  |
| `model` | `string` |  |  |
| `pid` | `integer` | はい |  |
| `seq` | `integer` | はい |  |
| `session` | `{ path?: string, resume?: string[], session_id?: string }` | はい |  |
| `state` | `"idle" \| "working" \| "blocked"` | はい |  |

### subagent

#### `subagent.cancel`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `subagent_id` | `string` | はい |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `object`

#### `subagent.get`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `subagent_id` | `string` | はい |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `ended_at_ms` | `integer` |  |  |
| `error` | `{ code: string, data?: any, message: string }` |  |  |
| `error_kind` | `string` |  |  |
| `label` | `string` |  |  |
| `name` | `string` |  |  |
| `parent_session_id` | `string` |  |  |
| `run_id` | `string` |  |  |
| `seq` | `integer` |  |  |
| `started_at_ms` | `integer` | はい |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled" \| "interrupted"` | はい |  |
| `subagent_id` | `string` | はい | Unique identifier (child session id) of the subagent. |
| `task` | `string` |  |  |

#### `subagent.list`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `cursor` | `string` |  |  |
| `limit` | `integer` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `object[]` | はい |  |
| `next_cursor` | `string` |  |  |

#### `subagent.list_types`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `object[]` | はい |  |

#### `subagent.register_type`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `definition` | `object` | はい |  |
| `replace` | `boolean` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `registration_id` | `string` | はい |  |

#### `subagent.start`

| フィールド | 型 | 必須 | 説明 |
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
| `prompt` | `string` | はい |  |
| `run_id` | `string` |  |  |
| `sections` | `object[]` |  |  |
| `seq` | `integer` |  |  |
| `system` | `string` |  |  |
| `task` | `string` |  |  |
| `tools` | `string[]` |  |  |
| `type` | `string` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `ended_at_ms` | `integer` |  |  |
| `error` | `{ code: string, data?: any, message: string }` |  |  |
| `error_kind` | `string` |  |  |
| `label` | `string` |  |  |
| `name` | `string` |  |  |
| `parent_session_id` | `string` |  |  |
| `run_id` | `string` |  |  |
| `seq` | `integer` |  |  |
| `started_at_ms` | `integer` | はい |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled" \| "interrupted"` | はい |  |
| `subagent_id` | `string` | はい | Unique identifier (child session id) of the subagent. |
| `task` | `string` |  |  |

#### `subagent.steer`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `subagent_id` | `string` | はい |  |
| `text` | `string` | はい |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `object`

#### `subagent.wait`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `subagent_id` | `string` | はい |  |
| `timeout_ms` | `integer` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `error` | `{ code: string, data?: any, message: string }` |  |  |
| `error_kind` | `string` |  |  |
| `messages` | `object[]` |  |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled" \| "interrupted"` | はい |  |
| `subagent_id` | `string` | はい | Unique identifier (child session id) of the subagent. |
| `text` | `string` |  |  |
| `timed_out` | `boolean` | はい |  |
| `usage` | `object` |  |  |

### terminal

#### `terminal.close`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `grace_ms` | `integer` |  | Optional SIGTERM grace period in milliseconds (0..30000, default 3000). |
| `terminal_id` | `string` | はい | Owned terminal identifier returned by `terminal.create`. |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `mare.TerminalSessionSnapshot`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `alternate_screen` | `boolean` |  |  |
| `cols` | `integer` | はい |  |
| `command_exit_code` | `integer` |  |  |
| `exit_code` | `integer` |  |  |
| `exit_signal` | `integer` |  |  |
| `name` | `string` |  |  |
| `output_offset` | `integer` | はい |  |
| `pid` | `integer` |  |  |
| `prompt_seq` | `integer` |  |  |
| `rows` | `integer` | はい |  |
| `screen` | `string` |  |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled"` | はい |  |
| `terminal_id` | `string` | はい |  |
| `truncated` | `boolean` | はい |  |
| `view_truncated` | `boolean` |  |  |

#### `terminal.create`

| フィールド | 型 | 必須 | 説明 |
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
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `mare.TerminalSessionSnapshot`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `alternate_screen` | `boolean` |  |  |
| `cols` | `integer` | はい |  |
| `command_exit_code` | `integer` |  |  |
| `exit_code` | `integer` |  |  |
| `exit_signal` | `integer` |  |  |
| `name` | `string` |  |  |
| `output_offset` | `integer` | はい |  |
| `pid` | `integer` |  |  |
| `prompt_seq` | `integer` |  |  |
| `rows` | `integer` | はい |  |
| `screen` | `string` |  |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled"` | はい |  |
| `terminal_id` | `string` | はい |  |
| `truncated` | `boolean` | はい |  |
| `view_truncated` | `boolean` |  |  |

#### `terminal.list`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `mare.TerminalSessionSnapshot[]` | はい |  |

#### `terminal.read`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `count` | `integer` |  |  |
| `max_bytes` | `integer` |  | Maximum output bytes to return (1..4194304, default 262144). Applies to both incremental `output_offset` slicing (UTF-8 safe, floor one char) and VT page mode as well as `screen`; sets `truncated` true when capped. |
| `offset` | `integer` |  |  |
| `output_offset` | `integer` |  |  |
| `screen` | `boolean` |  |  |
| `terminal_id` | `string` | はい | Owned terminal identifier returned by `terminal.create`. |
| `wait_ms` | `integer` |  | Optional bounded wait in milliseconds (0..60000). |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `alternate_screen` | `boolean` |  |  |
| `cols` | `integer` | はい |  |
| `command_exit_code` | `integer` |  |  |
| `exit_code` | `integer` |  |  |
| `exit_signal` | `integer` |  |  |
| `line_begin` | `integer` | はい |  |
| `line_end` | `integer` | はい |  |
| `name` | `string` |  |  |
| `next_output_offset` | `integer` | はい |  |
| `output_offset` | `integer` | はい |  |
| `pid` | `integer` |  |  |
| `prompt_seq` | `integer` |  |  |
| `rows` | `integer` | はい |  |
| `screen` | `string` |  |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled"` | はい |  |
| `terminal_id` | `string` | はい |  |
| `text` | `string` | はい |  |
| `total_lines` | `integer` | はい |  |
| `truncated` | `boolean` | はい |  |
| `view_truncated` | `boolean` |  |  |

#### `terminal.resize`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `cols` | `integer` | はい | Terminal columns (1..500, default 80). |
| `rows` | `integer` | はい | Terminal rows (1..200, default 24). |
| `terminal_id` | `string` | はい | Owned terminal identifier returned by `terminal.create`. |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `object`

#### `terminal.send`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `max_bytes` | `integer` |  | Maximum output bytes to return (1..4194304, default 262144). Output is truncated at a UTF-8 boundary (always returning at least one character if non-empty) and sets `truncated` true; when `screen` is true, rendered screen is head-trimmed to fit and sets `view_truncated`. |
| `screen` | `boolean` |  |  |
| `submit` | `boolean` |  |  |
| `terminal_id` | `string` | はい | Owned terminal identifier returned by `terminal.create`. |
| `text` | `string` |  |  |
| `wait_ms` | `integer` |  | Optional bounded wait in milliseconds (0..60000). |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `alternate_screen` | `boolean` |  |  |
| `cols` | `integer` | はい |  |
| `command_exit_code` | `integer` |  |  |
| `exit_code` | `integer` |  |  |
| `exit_signal` | `integer` |  |  |
| `name` | `string` |  |  |
| `next_output_offset` | `integer` | はい |  |
| `output` | `string` | はい |  |
| `output_offset` | `integer` | はい |  |
| `pid` | `integer` |  |  |
| `prompt_seq` | `integer` |  |  |
| `rows` | `integer` | はい |  |
| `screen` | `string` |  |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled"` | はい |  |
| `terminal_id` | `string` | はい |  |
| `timed_out` | `boolean` | はい |  |
| `truncated` | `boolean` | はい |  |
| `view_truncated` | `boolean` |  |  |

#### `terminal.signal`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `signal` | `"SIGINT" \| "SIGTERM" \| "SIGKILL" \| "SIGTSTP" \| "SIGHUP"` | はい |  |
| `terminal_id` | `string` | はい | Owned terminal identifier returned by `terminal.create`. |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `object`

### theme

#### `theme.cancel_preview`

パラメーターなし。

結果: `mare.ThemeSnapshot`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `active` | `string` | はい |  |
| `appearance` | `"dark" \| "light"` | はい |  |
| `component` | `any` | はい |  |
| `dark` | `string` | はい |  |
| `icons` | `map&lt;string, string&gt;` | はい |  |
| `light` | `string` | はい |  |
| `preference` | `string` | はい |  |
| `preview` | `string` |  |  |
| `revision` | `integer` | はい |  |
| `styles` | `map&lt;string, object&gt;` | はい |  |
| `syntax` | `string` |  |  |

#### `theme.get`

パラメーターなし。

結果: `mare.ThemeSnapshot`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `active` | `string` | はい |  |
| `appearance` | `"dark" \| "light"` | はい |  |
| `component` | `any` | はい |  |
| `dark` | `string` | はい |  |
| `icons` | `map&lt;string, string&gt;` | はい |  |
| `light` | `string` | はい |  |
| `preference` | `string` | はい |  |
| `preview` | `string` |  |  |
| `revision` | `integer` | はい |  |
| `styles` | `map&lt;string, object&gt;` | はい |  |
| `syntax` | `string` |  |  |

#### `theme.list`

パラメーターなし。

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `{ label: string, name: string, origin: string, variants: "dark" \| "light"[] }[]` | はい |  |

#### `theme.preview`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `theme` | `string` | はい |  |

結果: `mare.ThemeSnapshot`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `active` | `string` | はい |  |
| `appearance` | `"dark" \| "light"` | はい |  |
| `component` | `any` | はい |  |
| `dark` | `string` | はい |  |
| `icons` | `map&lt;string, string&gt;` | はい |  |
| `light` | `string` | はい |  |
| `preference` | `string` | はい |  |
| `preview` | `string` |  |  |
| `revision` | `integer` | はい |  |
| `styles` | `map&lt;string, object&gt;` | はい |  |
| `syntax` | `string` |  |  |

#### `theme.register`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `definition` | `object` | はい |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `registration_id` | `string` | はい |  |

#### `theme.register_override`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `layer_id` | `string` | はい |  |
| `styles` | `map&lt;string, object&gt;` | はい |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `registration_id` | `string` | はい |  |

#### `theme.select`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `save` | `boolean` |  |  |
| `theme` | `string` | はい |  |

結果: `mare.ThemeSnapshot`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `active` | `string` | はい |  |
| `appearance` | `"dark" \| "light"` | はい |  |
| `component` | `any` | はい |  |
| `dark` | `string` | はい |  |
| `icons` | `map&lt;string, string&gt;` | はい |  |
| `light` | `string` | はい |  |
| `preference` | `string` | はい |  |
| `preview` | `string` |  |  |
| `revision` | `integer` | はい |  |
| `styles` | `map&lt;string, object&gt;` | はい |  |
| `syntax` | `string` |  |  |

### tools

#### `tools.register`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `access` | `{ kind: "file", op: "read" \| "write" \| "create" \| "delete", path_arg: string, recursive: boolean }[]` |  |  |
| `concurrency_safe` | `boolean` |  |  |
| `description` | `string` | はい |  |
| `exposure` | `"direct" \| "deferred" \| "nested"` |  |  |
| `guidelines` | `string[]` |  |  |
| `idempotent` | `boolean` |  |  |
| `name` | `string` | はい |  |
| `parameters` | `object` | はい |  |
| `side_effect` | `"read" \| "write" \| "external"` |  |  |
| `snippet` | `string` |  |  |
| `timeout_ms` | `integer` |  |  |
| `callbacks` | `"execute"[]` | はい |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `registration_id` | `string` | はい |  |

### ui

#### `ui.attach`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |
| `responder` | `boolean \| null` |  |  |
| `takeover` | `string[]` |  | (default `[]`) |
| `viewport` | `UiViewport \| null` |  |  |

結果: `UiAttachResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |
| `session_epoch` | `string` | はい |  |
| `responder` | `boolean` | はい |  |
| `takeover` | `string[]` | はい |  |
| `snapshot` | `UiShellSnapshot` | はい |  |

#### `ui.call_keymap`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `action` | `string` |  |  |
| `context` | `string` |  |  |
| `key` | `string` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `action` | `string` |  |  |
| `context` | `string` |  |  |
| `declined` | `boolean` | はい |  |
| `handled` | `boolean` | はい |  |
| `masked` | `boolean` | はい |  |
| `reason` | `string` |  |  |

#### `ui.cancel_markdown`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `document_id` | `string` | はい |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `object`

#### `ui.delete_keymap`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `context` | `string` | はい |  |
| `key` | `string` | はい |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `context` | `string` | はい |  |
| `key` | `string` | はい |  |

#### `ui.delete_markdown`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `document_id` | `string` | はい |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `object`

#### `ui.detach`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |

結果: `UiDetachResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |

#### `ui.get_keymap`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `context` | `string` |  | Optional context name to filter by. |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `actions` | `{ contexts?: string[], core?: boolean, desc?: string, essential?: boolean, keys?: string[], name: string }[]` | はい |  |
| `bindings` | `object[]` | はい |  |
| `conflicts` | `{ missing_essential: { action: string, context: string }[], unbound_essential: { action: string, context: string }[] }` | はい |  |
| `contexts` | `{ bindable: boolean, chain: string[], core: boolean, name: string, parents: string[] }[]` | はい |  |

#### `ui.get_markdown`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `document_id` | `string` | はい |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `mare.MarkdownSnapshot`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `diagnostic` | `string` |  |  |
| `document_id` | `string` | はい |  |
| `enhanced` | `boolean` | はい |  |
| `expanded_node` | `mare.Node` | はい |  |
| `node` | `mare.Node` | はい |  |
| `pending` | `integer` | はい |  |
| `render_seq` | `integer` | はい |  |
| `revision` | `integer` | はい |  |
| `session_epoch` | `string` |  |  |
| `session_id` | `string` |  |  |
| `source` | `string` | はい |  |
| `supports_artifact_images` | `boolean` | はい |  |

#### `ui.get_select_preview`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `index` | `integer` | はい |  |
| `select_id` | `string` | はい |  |
| `tab` | `integer` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `preview` | `string \| mare.Node` |  |  |

#### `ui.invalidate`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `component` | `string` |  | Widget (`widget:<id>` or widget id) or status segment id to redraw. |
| `viewport` | `{ height?: integer, supports_artifact_images?: boolean, supports_images?: boolean, width?: integer }` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `object`

#### `ui.invalidate_presentations`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `object`

#### `ui.list_markdown_presenters`

パラメーターなし。

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `{ language: string, preview: boolean, requires_artifact_images: boolean, timeout_ms: integer }[]` | はい |  |

#### `ui.register_entry_renderer`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `kind` | `string` | はい |  |
| `callbacks` | `"render"[]` | はい |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `registration_id` | `string` | はい |  |

#### `ui.register_markdown_transform`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `message_types` | `"user" \| "assistant" \| "assistant-reasoning" \| "custom" \| "summary" \| "changelog"[]` |  |  |
| `callbacks` | `"transform"[]` | はい |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `registration_id` | `string` | はい |  |

#### `ui.render_markdown`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `base_node` | `mare.Node` |  |  |
| `document_id` | `string` | はい |  |
| `replay` | `boolean` |  |  |
| `source` | `string` | はい |  |
| `viewport` | `{ height?: integer, supports_artifact_images?: boolean, supports_images?: boolean, width?: integer }` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果: `mare.MarkdownSnapshot`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `diagnostic` | `string` |  |  |
| `document_id` | `string` | はい |  |
| `enhanced` | `boolean` | はい |  |
| `expanded_node` | `mare.Node` | はい |  |
| `node` | `mare.Node` | はい |  |
| `pending` | `integer` | はい |  |
| `render_seq` | `integer` | はい |  |
| `revision` | `integer` | はい |  |
| `session_epoch` | `string` |  |  |
| `session_id` | `string` |  |  |
| `source` | `string` | はい |  |
| `supports_artifact_images` | `boolean` | はい |  |

#### `ui.send_editor`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `attachments` | `object[]` |  |  |
| `mode` | `"prompt" \| "steer" \| "follow_up"` |  |  |
| `text` | `string` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `handled` | `boolean` | はい |  |
| `result` | `object` |  |  |
| `revision` | `integer` | はい |  |

#### `ui.set_external_editor`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `action` | `"start" \| "commit" \| "cancel"` | はい |  |
| `text` | `string` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `program` | `string` |  |  |
| `revision` | `integer` | はい |  |
| `running` | `boolean` | はい |  |
| `text` | `string` | はい |  |

#### `ui.set_keymap`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `action` | `string \| boolean` | はい |  |
| `context` | `string` | はい |  |
| `desc` | `string` |  |  |
| `key` | `string` | はい |  |
| `priority` | `number` |  |  |
| `unique` | `boolean` |  |  |
| `when` | `string[]` |  |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `registration_id` | `string` | はい |  |

#### `ui.transform_markdown`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `available_width` | `integer` |  |  |
| `created_at_ms` | `integer` |  |  |
| `is_streaming` | `boolean` |  |  |
| `message_id` | `string` |  |  |
| `message_type` | `"user" \| "assistant" \| "assistant-reasoning" \| "custom" \| "summary" \| "changelog"` |  |  |
| `text` | `string` | はい |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `text` | `string` | はい |  |

#### `ui.update_editor`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `attachments` | `{ attachment_id: string, height?: integer, label?: string, mime_type?: string, width?: integer }[]` |  |  |
| `image_markers` | `string[]` |  |  |
| `offset` | `integer` |  |  |
| `shell_mode` | `boolean` |  |  |
| `text` | `string` | はい |  |
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `attachments` | `{ attachment_id: string, height?: integer, label?: string, mime_type?: string, width?: integer }[]` |  |  |
| `external_editor_running` | `boolean` | はい |  |
| `image_markers` | `string[]` |  |  |
| `offset` | `integer` | はい |  |
| `revision` | `integer` | はい |  |
| `shell_mode` | `boolean` | はい |  |
| `text` | `string` | はい |  |

### usage

#### `usage.get`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |
| `expected_epoch` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `context_tokens` | `integer` | はい |  |
| `rate_limits` | `map&lt;string, { credits?: string, limit_name?: string, primary?: object, reached?: string, received_at_ms: integer, secondary?: object }&gt;` | はい |  |
| `totals` | `object` | はい |  |

### web

#### `web.fetch`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `timeout_ms` | `integer` |  |  |
| `url` | `string` | はい |  |

結果: `mare.WebFetchResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `content` | `string` | はい |  |
| `content_type` | `string` |  |  |
| `requested_url` | `string` | はい |  |
| `status` | `integer` |  |  |
| `truncated` | `boolean` |  |  |
| `url` | `string` |  |  |

#### `web.list_providers`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `kind` | `"search" \| "fetch"` | はい |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `{ available: boolean, provider_id: string, selected: boolean, unconfined: boolean }[]` | はい |  |
| `reason` | `string` |  |  |

#### `web.register_fetch`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `provider_id` | `string` | はい |  |
| `unconfined` | `boolean` |  |  |
| `callbacks` | `"fetch"[]` | はい |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `registration_id` | `string` | はい |  |

#### `web.register_search`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `provider_id` | `string` | はい |  |
| `unconfined` | `boolean` |  |  |
| `callbacks` | `"search"[]` | はい |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `registration_id` | `string` | はい |  |

#### `web.search`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `queries` | `string[]` | はい |  |
| `timeout_ms` | `integer` |  |  |

結果: `mare.WebSearchResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `content` | `string` |  |  |
| `data` | `{ published_at_ms?: integer, snippet?: string, summary?: string, title?: string, url: string }[]` | はい |  |
| `truncated` | `boolean` |  |  |

### webserver

#### `webserver.get`

パラメーターなし。

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `host` | `string` |  |  |
| `port` | `integer` |  |  |
| `running` | `boolean` | はい |  |
| `tls` | `boolean` |  |  |
| `trusted_hosts` | `string[]` |  |  |
| `url` | `string` |  |  |
| `ws_url` | `string` |  |  |

#### `webserver.list_assets`

パラメーターなし。

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `{ asset_id: string, dir: string, fallback?: string, index?: string, plugin?: string, prefix: string }[]` | はい |  |

#### `webserver.list_routes`

パラメーターなし。

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `data` | `{ method: string, owner_session_id?: string, path: string, plugin?: string, route_id: string }[]` | はい |  |

#### `webserver.register_assets`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `dir` | `string` | はい |  |
| `fallback` | `string` |  |  |
| `index` | `string` |  |  |
| `prefix` | `string` |  |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `registration_id` | `string` | はい |  |

#### `webserver.register_route`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `method` | `"GET" \| "POST" \| "PUT" \| "PATCH" \| "DELETE" \| "HEAD" \| "ANY"` |  |  |
| `path` | `string` | はい |  |
| `callbacks` | `"handle"[]` | はい |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `registration_id` | `string` | はい |  |

### workspace

#### `workspace.get`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `cwd` | `string` | はい |  |

結果:

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `dirs` | `string[]` | はい |  |
| `root` | `string` | はい |  |
| `trusted` | `boolean` | はい |  |

#### `workspace.set_trust`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `save` | `boolean` |  |  |
| `trusted` | `boolean` | はい |  |
| `cwd` | `string` | はい |  |

結果: `object`

## 通知（mare → クライアント）

### `$/cancel_request`

Cancels an outstanding mare-to-host request. Late replies to this ID are ignored.

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `id` | `RequestId` | はい |  |

### `dropped`

Drop count for unretained `lossy` or refused events omitted from external stream consumers.

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `session_epoch` | `string \| null` |  |  |
| `count` | `integer ≥ 0` | はい |  |

### `event`

Envelope of an `event` notification: unified across RPC and headless streams.

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `type` | `string` | はい | kernel event name, e.g. `turn/started`, `item/delta`, `item/completed`, `turn/completed` |
| `session_id` | `string \| null` |  | session the event belongs to |
| `session_epoch` | `string \| null` |  | opaque in-memory epoch identifier that rotates on every session create, load, or switch |
| `seq` | `integer ≥ 0` |  | per-session, per-epoch sequence number (1-based, contiguous) (default `0`) |
| `turn_id` | `string \| null` |  | run (prompt) the event belongs to — matches `agent.prompt` `turn_id` |
| `ignorable` | `boolean \| null` |  | whether a client that does not recognize this event type may skip it |
| `data` | `any` |  |  |

### `fs/changed`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `watch_id` | `string` | はい |  |
| `changed_paths` | `string[]` | はい |  |
| `truncated` | `boolean` |  |  |

### `lagged`

Sequence range omitted when an ordinary consumer cursor fell behind the bounded event log.

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `session_epoch` | `string \| null` |  |  |
| `from_seq` | `integer ≥ 0` | はい |  |
| `to_seq` | `integer ≥ 0` | はい |  |

### `peer.message_status`

`peer.message_status` notification (mare → peer): the receiving user's decision on a held
message.

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `message_id` | `string` | はい |  |
| `status` | `string` | はい | "delivered" \| "rejected" |
| `reason` | `string \| null` |  |  |

### `snapshot`

`snapshot` notification (mare → host): emitted immediately after an `events.subscribe` reply
with `mode = "baseline"`, and immediately after `lagged` when a live cursor is evicted.

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `session_epoch` | `string` | はい |  |
| `seq` | `integer ≥ 0` | はい | Ring tail sequence number `S` at the moment of the non-yielding cut; following events start at `S + 1`. |
| `baseline_id` | `string \| null` |  | Opaque handle for paging `parts` via `baseline.read`. |
| `retained` | `object` |  | Retained state (`events`, `status`, `queue`, `active_turn`, `active_items` skeletons, pending approvals/interactions). (default `{}`) |
| `parts` | `string[]` |  | Baseline parts available to page via `baseline.read` (e.g. `["entries", "active_items"]`). (default `[]`) |

### `stream.failed`

`event` notification params — exactly one `mare -p --json` line.
An event stream is incomplete and this connection will close.

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `error` | `string` | はい |  |

### `ui.editor_config`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `prompt` | `string \| null` |  |  |
| `placeholder` | `string \| null` |  |  |
| `busy_placeholder` | `string \| null` |  |  |
| `border` | `string \| null` |  |  |

### `ui.editor_hint`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `text` | `string` | はい |  |

### `ui.editor_state`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `text` | `string` | はい |  |
| `cursor` | `integer ≥ 0` | はい |  |
| `shell_mode` | `boolean` |  | (default `false`) |
| `image_markers` | `string[]` |  |  |
| `attachments` | `UiEditorAttachment[]` |  |  |
| `external_editor_running` | `boolean` |  | (default `false`) |
| `revision` | `integer ≥ 0` | はい |  |

### `ui.markdown_changed`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `document_id` | `string` | はい |  |
| `source` | `string` | はい |  |
| `revision` | `integer ≥ 0` | はい |  |
| `render_seq` | `integer ≥ 0` | はい |  |
| `session_id` | `string \| null` |  |  |
| `session_epoch` | `string \| null` |  |  |
| `pending` | `integer ≥ 0` | はい |  |
| `enhanced` | `boolean` | はい |  |
| `supports_artifact_images` | `boolean` | はい |  |
| `diagnostic` | `string \| null` |  |  |
| `node` | `any` | はい | Existing Node description. Image bytes become bounded canonical base64 `data`; no paths, callbacks, tokens, or userdata. Deserialize via mare.ui.node_from_wire in a Lua frontend. |
| `expanded_node` | `any` | はい | Same Markdown document with each enhanced fence followed by its complete source. |

### `ui.notify`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `text` | `string` | はい |  |
| `level` | `string \| null` |  | info \| success \| warning \| error |

### `ui.overlay`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `title` | `string` | はい |  |
| `node` | `any` |  |  |
| `open` | `boolean` | はい |  |
| `context` | `string \| null` |  |  |

### `ui.set_editor`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `text` | `string` | はい |  |
| `keep_draft` | `boolean` |  | (default `false`) |
| `prepend` | `boolean` |  | (default `false`) |

### `ui.status`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `status_id` | `string` | はい |  |
| `align` | `string` | はい |  |
| `order` | `integer` |  | (default `0`) |
| `keep` | `boolean` |  | (default `false`) |
| `node` | `any` |  |  |

### `ui.widget`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `widget_id` | `string` | はい |  |
| `slot` | `string` | はい |  |
| `order` | `integer` |  | (default `0`) |
| `node` | `any` |  |  |

### `ui/component-updated`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい |  |
| `component` | `string` | はい |  |
| `payload` | `any` |  |  |

### `ui/select-closed`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `select_id` | `string` | はい |  |

### `ui/select-updated`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `select_id` | `string` | はい |  |
| `items` | `UiSelectItem[]` |  |  |
| `tabs` | `UiSelectTab[]` |  |  |

## 通知（クライアント → mare）

### `$/cancel_request`

Cancels an outstanding mare-to-host request. Late replies to this ID are ignored.

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `id` | `RequestId` | はい |  |

### `callback.update`

`callback.update`: client → server notification streaming a partial update for an in-flight
`callback.call` request identified by `id`.

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `id` | `RequestId` | はい |  |
| `partial` | `any` | はい |  |

## コールバック（mare → クライアントへのリクエスト）

### `approval.request`

`approval.request` (mare → host): a tool call or a network host needs approval.
No answer (timeout, disconnect, error, invalid result) counts as `deny`.

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい | session that needs approval |
| `turn_id` | `string \| null` |  | active turn id, when known |
| `kind` | `string` | はい | "tool" or "network" |
| `approval_id` | `string \| null` |  | unique approval lifecycle id (pairs with `approval/requested` and `approval/resolved`) |
| `tool_call_id` | `string \| null` |  | tool call id (kind = tool) |
| `tool` | `string \| null` |  | tool name (kind = tool) |
| `arguments` | `any` |  | the call's arguments (kind = tool) |
| `command` | `string \| null` |  | shell command, when the tool runs one |
| `diff` | `string \| null` |  | proposed change (patch / edit), when the tool writes files |
| `host` | `string \| null` |  | host:port (kind = network) |
| `cwd` | `string` | はい |  |
| `reason` | `string \| null` |  | why approval is needed (from the policy vote) |
| `side_effect` | `string \| null` |  | "read" \| "write" \| "external" |
| `unconfined` | `boolean` |  | runs outside the local sandbox (MCP / RPC-host tools) (default `false`) |
| `proposed_amendment` | `any` |  | a rule the policy proposes for `allow_amend` (opaque to core) |
| `requested_permissions` | `PermissionProfile \| null` |  | Explicit permission increment requested by this tool call (read/write paths, network hosts). |
| `scope` | `PermissionGrantScope \| null` |  | Requested maximum lifetime, when this is a structured permission request. |

結果: `ApprovalRequestResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `decision` | `ApprovalDecision` | はい |  |
| `permissions` | `PermissionProfile` |  | Optional structured subset of the requested read/write/network increment. |
| `scope` | `PermissionGrantScope` |  | Lifetime of the grant: turn or session (the request remains an upper bound). |
| `amendment` | `any` |  | with `allow_amend`: the rule to persist (opaque to core) |
| `reason` | `string \| null` |  | shown to the model when denied |

### `callback.call`

`callback.call`: server → client request invoking one contract callback on a registration the
client holds. Arguments match the callback's params schema; result is
checked against its result schema.

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `registration_id` | `string` | はい |  |
| `callback` | `string` | はい |  |
| `session_id` | `string \| null` |  |  |
| `turn_id` | `string \| null` |  |  |
| `tool_call_id` | `string \| null` |  |  |
| `caller_id` | `string \| null` |  |  |
| `arguments` | `any` | はい |  |

結果: `any`

### `mcp.elicitation`

`mcp.elicitation` (mare → host): an MCP server requested user input or URL confirmation.

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string` | はい | session that requested elicitation |
| `turn_id` | `string \| null` |  | active turn id, when known |
| `server` | `string` | はい |  |
| `request_id` | `any` |  |  |
| `mode` | `string \| null` |  |  |
| `message` | `string` |  | (default `""`) |
| `requested_schema` | `any` |  |  |
| `url` | `string \| null` |  |  |
| `elicitation_id` | `string \| null` |  |  |
| `_meta` | `any` |  |  |

結果: `McpElicitationResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `action` | `McpElicitationAction` | はい |  |
| `content` | `any` |  |  |

### `ui.confirm`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `prompt` | `string` | はい |  |
| `default` | `boolean \| null` |  |  |

結果: `UiConfirmResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `confirmed` | `boolean \| null` |  | true/false when answered; null when cancelled or interrupted |
| `reason` | `string \| null` |  | Dismissal reason when cancelled (`"cancelled"` \| `"interrupted"`) |

### `ui.editor`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `title` | `string` |  | (default `""`) |
| `prefill` | `string` |  | (default `""`) |

結果: `UiTextPromptResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `text` | `string \| null` |  | null when cancelled |
| `reason` | `string \| null` |  | Dismissal reason when cancelled (`"cancelled"` \| `"interrupted"`) |

### `ui.input`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `title` | `string` |  | (default `""`) |
| `placeholder` | `string` |  | (default `""`) |
| `default` | `string \| null` |  |  |
| `secret` | `boolean` |  | (default `false`) |

結果: `UiTextPromptResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `text` | `string \| null` |  | null when cancelled |
| `reason` | `string \| null` |  | Dismissal reason when cancelled (`"cancelled"` \| `"interrupted"`) |

### `ui.questions`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `title` | `string \| null` |  |  |
| `questions` | `UiQuestionItem[]` | はい |  |

結果: `UiQuestionsResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `answers` | `map&lt;string, UiQuestionAnswer&gt;` |  | (default `{}`) |
| `dismissed` | `boolean \| null` |  |  |
| `reason` | `string \| null` |  |  |
| `note` | `string \| null` |  |  |

### `ui.select`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `select_id` | `string \| null` |  |  |
| `title` | `string` |  | (default `""`) |
| `title_right` | `any` |  |  |
| `body` | `any[]` |  | Explanatory lines shown above the options (strings or wire `Node` objects). |
| `items` | `UiSelectItem[]` | はい |  |
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

結果: `UiSelectResult`

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `index` | `integer \| null` |  |  |
| `selected_indices` | `integer ≥ 0[] \| null` |  | 0-based checked item indices for a multi-select; an empty array is a valid answer. |
| `tab` | `integer \| null` |  |  |
| `key` | `string \| null` |  |  |
| `filter` | `string \| null` |  |  |
| `reason` | `string \| null` |  | Dismissal reason when cancelled (`"cancelled"` \| `"interrupted"`) |

## エラーコード

エラーの `data.code` は何が失敗したかを、`data.data` は追加データを表します。サービス独自のコードは `<サービス>.<名前>` で、サービスの契約が宣言します（`services.list` に表示）。それ以外はすべて `internal` です。

| `data.code` | 番号 | 意味 |
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
