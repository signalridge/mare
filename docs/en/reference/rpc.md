# RPC reference

Every method, notification and callback of `mare --rpc`, taken from `mare rpc schema`. Field names use `snake_case` across the protocol. How a session runs over RPC, with examples, is in [JSON-RPC](../integrate/rpc.md).

> This page is generated from the code by `scripts/gen_docs.py`. Do not edit it by hand.

Protocol version: **3.0**

## Methods (client → mare)

### agent

#### `agent.cancel`

| Field | Type | Required | Description |
|---|---|---|---|
| `clear_queue` | `boolean` |  |  |
| `mode` | `"cancel" \| "halt"` |  |  |
| `reason` | `string` |  | Cancellation cause (`user`, `parent`, `hook`, `disposed`) or free-form note (classified as `user`). |
| `request_id` | `string` |  |  |
| `turn_id` | `string` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `duplicate` | `boolean` |  |  |
| `removed` | `{ content?: object[], images: { data?: string, mime: string, path?: string, size?: integer }[], input_context?: any, input_id: string, lane: "steer" \| "follow_up", text: string }[]` | yes |  |
| `turn_id` | `string` |  |  |

#### `agent.continue`

| Field | Type | Required | Description |
|---|---|---|---|
| `effort` | `string` |  |  |
| `tools` | `string \| object[]` |  |  |
| `output_schema` | `object` |  |  |
| `source` | `string` |  |  |
| `system` | `string` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `turn_id` | `string` | yes |  |

#### `agent.delete_queued`

| Field | Type | Required | Description |
|---|---|---|---|
| `input_id` | `string` | yes |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `object`

#### `agent.get`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `activity` | `{ extra: object, extra_errors: object }` |  |  |
| `busy` | `boolean` | yes |  |
| `effort` | `string` |  |  |
| `interrupted` | `boolean` |  |  |
| `message_count` | `integer` |  |  |
| `model` | `string` |  |  |
| `queued` | `integer` | yes |  |
| `session_id` | `string` |  |  |
| `turn_cancelled` | `boolean` |  |  |
| `turn_id` | `string` |  |  |

#### `agent.list_messages`

| Field | Type | Required | Description |
|---|---|---|---|
| `cursor` | `string` |  |  |
| `limit` | `integer` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `object[]` | yes |  |
| `next_cursor` | `string` |  |  |

#### `agent.list_queue`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `{ content?: object[], images: { data?: string, mime: string, path?: string, size?: integer }[], input_context?: any, input_id: string, lane: "steer" \| "follow_up", text: string }[]` | yes |  |

#### `agent.prompt`

| Field | Type | Required | Description |
|---|---|---|---|
| `effort` | `string` |  |  |
| `when_busy` | `"queue" \| "reject"` |  |  |
| `input` | `string \| object[]` | yes |  |
| `output_schema` | `object` |  |  |
| `request_id` | `string` |  |  |
| `source` | `string` |  |  |
| `system` | `string` |  |  |
| `tools` | `string \| object[]` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `duplicate` | `boolean` |  |  |
| `input_id` | `string` |  |  |
| `status` | `"started" \| "queued"` | yes |  |
| `turn_id` | `string` |  |  |

#### `agent.steer`

| Field | Type | Required | Description |
|---|---|---|---|
| `expected_turn_id` | `string` |  |  |
| `input` | `string \| object[]` | yes |  |
| `request_id` | `string` |  |  |
| `source` | `string` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `duplicate` | `boolean` |  |  |
| `input_id` | `string` | yes |  |

#### `agent.update_queued`

| Field | Type | Required | Description |
|---|---|---|---|
| `input` | `string \| object[]` |  |  |
| `input_id` | `string` | yes |  |
| `lane` | `"steer" \| "follow_up"` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `content` | `object[]` |  | Optional ordered content blocks with image bytes stripped (preserved on cancel). |
| `images` | `{ data?: string, mime: string, path?: string, size?: integer }[]` | yes | Queued image attachments. |
| `input_context` | `any` |  | Opaque caller-supplied JSON metadata preserved with the queued input. |
| `input_id` | `string` | yes | Stable queued input id. |
| `lane` | `"steer" \| "follow_up"` | yes | Delivery lane (`steer` or `follow_up`). |
| `text` | `string` | yes | Queued input text. |

#### `agent.wait`

| Field | Type | Required | Description |
|---|---|---|---|
| `timeout_ms` | `integer` |  |  |
| `turn_id` | `string` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `cancellation_cause` | `"user" \| "parent" \| "hook" \| "disposed"` |  | Origin of cancellation when `termination_reason` is `interrupted`. |
| `completed_at_ms` | `integer` |  | Unix epoch milliseconds when the turn finished. |
| `error` | `{ code: string, data?: any, message: string }` |  |  |
| `error_kind` | `string` |  | Classified failure kind when present. |
| `messages` | `object[]` |  | Session messages visible at turn end. |
| `not_sent` | `boolean` |  | True when the prompt failed before reaching the provider. |
| `save_warning` | `string` |  | Quarantine/save failure warning when another error took precedence. |
| `started_at_ms` | `integer` | yes | Unix epoch milliseconds when the turn started. |
| `status` | `"completed" \| "cancelled" \| "failed" \| "running"` | yes | Turn completion outcome (or `running` when `wait` timed out). |
| `structured` | `any` |  | Validated structured output when `output_schema` was supplied. |
| `termination_reason` | `"completed" \| "usage_limit" \| "interrupted" \| "aborted" \| "turn_limit" \| "error"` |  | Machine-readable turn termination outcome. |
| `text` | `string` |  | Final assistant text produced during the turn. |
| `timed_out` | `boolean` |  | True when `agent.wait` returned on `timeout_ms` before settlement. |
| `turn_id` | `string` | yes | Turn identifier. |
| `usage` | `object` |  | Aggregated token/cost usage for the turn. |

### baseline

#### `baseline.read`

Parameters for `baseline.read`.

| Field | Type | Required | Description |
|---|---|---|---|
| `baseline_id` | `string` | yes |  |
| `part` | `string` | yes |  |
| `cursor` | `string \| null` |  |  |
| `limit` | `integer \| null` |  | Optional maximum number of items to return in one page (&gt;= 1). |

Result: `BaselineReadResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `any[]` | yes |  |
| `next_cursor` | `string \| null` |  |  |

Errors: `baseline.stale`, `baseline.unknown_part`, `baseline.item_too_large`

### cells

#### `cells.cancel`

| Field | Type | Required | Description |
|---|---|---|---|
| `cell_id` | `string` | yes |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `object`

#### `cells.get`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `retains_state` | `boolean` | yes |  |
| `running` | `integer` | yes |  |
| `store_durable` | `boolean` | yes |  |
| `stored_keys` | `integer` | yes |  |

#### `cells.list`

| Field | Type | Required | Description |
|---|---|---|---|
| `cursor` | `string` |  |  |
| `limit` | `integer` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `{ cell_id: string, status: "running" \| "completed" \| "failed" \| "cancelled" }[]` | yes |  |
| `next_cursor` | `string` |  |  |

#### `cells.list_tools`

| Field | Type | Required | Description |
|---|---|---|---|
| `cursor` | `string` |  |  |
| `limit` | `integer` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `{ declaration: string, description: string, name: string, output_schema: object, parameters: object }[]` | yes |  |
| `next_cursor` | `string` |  |  |

#### `cells.reset`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `object`

#### `cells.start`

| Field | Type | Required | Description |
|---|---|---|---|
| `capabilities` | `string[]` |  |  |
| `cell_id` | `string` |  |  |
| `code` | `string` | yes |  |
| `max_output_tokens` | `integer` |  |  |
| `state_owner` | `string` |  |  |
| `timeout_ms` | `integer \| false` |  |  |
| `yield_time_ms` | `integer` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `mare.CellResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `cell_id` | `string` | yes |  |
| `content` | `any[]` | yes |  |
| `error` | `string` |  |  |
| `result` | `any` |  |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled"` | yes |  |
| `stop_unconfirmed` | `boolean` |  |  |

#### `cells.wait`

| Field | Type | Required | Description |
|---|---|---|---|
| `cell_id` | `string` | yes |  |
| `max_output_tokens` | `integer` |  |  |
| `terminate` | `boolean` |  |  |
| `yield_time_ms` | `integer` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `mare.CellResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `cell_id` | `string` | yes |  |
| `content` | `any[]` | yes |  |
| `error` | `string` |  |  |
| `result` | `any` |  |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled"` | yes |  |
| `stop_unconfirmed` | `boolean` |  |  |

### commands

#### `commands.get`

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | `string` | yes |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `alias_of` | `string` |  |  |
| `args` | `string` |  |  |
| `auto_execute` | `boolean` |  |  |
| `availability` | `"always" \| "idle" \| "session" \| "main" \| "running" \| "tui" \| "always" \| "idle" \| "session" \| "main" \| "running" \| "tui"[]` |  |  |
| `description` | `string` |  |  |
| `local_only` | `boolean` |  |  |
| `name` | `string` | yes |  |

#### `commands.list`

No parameters.

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `object[]` | yes |  |

#### `commands.register`

| Field | Type | Required | Description |
|---|---|---|---|
| `alias_of` | `string` |  |  |
| `args` | `string` |  |  |
| `auto_execute` | `boolean` |  |  |
| `availability` | `"always" \| "idle" \| "session" \| "main" \| "running" \| "tui" \| "always" \| "idle" \| "session" \| "main" \| "running" \| "tui"[]` |  |  |
| `description` | `string` |  |  |
| `local_only` | `boolean` |  |  |
| `name` | `string` | yes |  |
| `replace` | `boolean` |  |  |
| `callbacks` | `"complete" \| "run"[]` |  |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `registration_id` | `string` | yes |  |

#### `commands.run`

| Field | Type | Required | Description |
|---|---|---|---|
| `input` | `{ content?: object[] }` |  |  |
| `line` | `string` | yes |  |
| `session_id` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `result` | `any` |  |  |

### compact

#### `compact.run`

| Field | Type | Required | Description |
|---|---|---|---|
| `instructions` | `string` |  |  |
| `messages` | `object[]` |  |  |
| `reason` | `"manual"` |  |  |
| `summarize_only` | `boolean` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `dropped` | `integer` |  |  |
| `status` | `"compacted" \| "nothing_to_compact" \| "unchanged" \| "summarized"` | yes |  |
| `summary` | `string` |  |  |
| `tokens_after` | `integer` | yes |  |
| `tokens_before` | `integer` | yes |  |

### config

#### `config.describe`

Return redacted config metadata, provenance, and redacted warnings.

| Field | Type | Required | Description |
|---|---|---|---|
| `keys` | `string[] \| null` |  | option names or dotted paths (default: all) |

Result: `ConfigDescribeResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `options` | `map&lt;string, OptionDescriptor&gt;` | yes |  |
| `warnings` | `string[]` |  |  |

### connection

#### `connection.set_capabilities`

| Field | Type | Required | Description |
|---|---|---|---|
| `capabilities` | `StrictClientCapabilities` | yes |  |

Result: `SetCapabilitiesResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `capabilities` | `ClientCapabilities` | yes |  |

### events

#### `events.list`

No parameters.

Result: `EventsListResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `EventInfo[]` | yes |  |

#### `events.subscribe`

`events.subscribe`: subscribe to (or filter) a session's event stream.

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string \| null` |  | session id to subscribe to (required unless `scope = "all"` is explicitly set by the owner) |
| `session_epoch` | `string \| null` |  | last observed `session_epoch`; mismatches fall back to `mode = "baseline"` |
| `last_seq` | `integer \| null` |  | last delivered `seq` within `session_epoch` for resumptive replay |
| `scope` | `RegistrationScopeKind \| null` |  |  |
| `include` | `string[] \| null` |  |  |
| `exclude` | `string[] \| null` |  |  |

Result: `EventsSubscribeResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `mode` | `SubscribeMode` | yes |  |
| `session_epoch` | `string` | yes |  |
| `seq` | `integer ≥ 0` | yes | sequence number at which the stream continues (`next_seq` for `live`/`baseline`, `last_seq + 1` for `replay`) |

### flags

#### `flags.get`

`flags.get` params.

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | `string` | yes |  |

Result: `FlagsGetResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | `string` | yes |  |
| `registered` | `boolean` | yes |  |
| `value` | `any` |  |  |

#### `flags.list`

No parameters.

Result: `FlagsListResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `FlagEntry[]` | yes |  |

### fs

#### `fs.list`

| Field | Type | Required | Description |
|---|---|---|---|
| `path` | `string` | yes |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `{ file_name: string, is_directory: boolean, is_file: boolean }[]` | yes |  |

#### `fs.read`

| Field | Type | Required | Description |
|---|---|---|---|
| `max_bytes` | `integer` |  |  |
| `path` | `string` | yes |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data_base64` | `string` | yes |  |

#### `fs.stat`

| Field | Type | Required | Description |
|---|---|---|---|
| `path` | `string` | yes |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `created_at_ms` | `integer` | yes |  |
| `is_directory` | `boolean` | yes |  |
| `is_file` | `boolean` | yes |  |
| `mode` | `integer` |  |  |
| `modified_at_ms` | `integer` | yes |  |
| `nlink` | `integer` |  |  |
| `size` | `integer` | yes |  |

#### `fs.watch`

| Field | Type | Required | Description |
|---|---|---|---|
| `path` | `string` | yes |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `registration_id` | `string` | yes |  |

#### `fs.write`

| Field | Type | Required | Description |
|---|---|---|---|
| `atomic` | `boolean` |  |  |
| `create_new` | `boolean` |  |  |
| `data_base64` | `string` | yes |  |
| `path` | `string` | yes |  |

Result: `object`

### hooks

#### `hooks.subscribe`

| Field | Type | Required | Description |
|---|---|---|---|
| `event` | `string` | yes |  |
| `prepend` | `boolean` |  |  |
| `timeout_ms` | `integer` |  |  |
| `callbacks` | `"handle"[]` | yes |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `registration_id` | `string` | yes |  |

### `initialize`

| Field | Type | Required | Description |
|---|---|---|---|
| `protocol_version` | `string` | yes | Protocol version the host speaks (`major.minor`); the major version must match. |
| `client` | `ClientInfo \| null` |  |  |
| `capabilities` | `ClientCapabilities` |  | Callbacks the host can answer. Undeclared callbacks are never sent (defaults apply). (default `{"callbacks": false, "ui": false, "approvals": false, "elicitation": false}`) |
| `callback_timeout_ms` | `integer \| null` |  | Timeout for callbacks into the host in milliseconds (default 30000, clamped to 1..=3600000). |
| `approval_timeout_ms` | `integer \| null` |  | Optional human approval timeout in milliseconds; absent means wait without a deadline. |

Result: `InitializeResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `protocol_version` | `string` | yes |  |
| `server` | `ServerInfo` | yes |  |
| `capabilities` | `ServerCapabilities` | yes |  |
| `session` | `any` |  | Live `mare.Session` snapshot of the opened initial session, if any (`session_id` and `session_epoch` live on this object; session-scoped contract methods take `session_id`). |

### jobs

#### `jobs.background`

| Field | Type | Required | Description |
|---|---|---|---|
| `job_id` | `string` | yes |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `object`

#### `jobs.cancel`

| Field | Type | Required | Description |
|---|---|---|---|
| `job_id` | `string` | yes |  |
| `reason` | `string` |  |  |
| `silent` | `boolean` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `object`

#### `jobs.list`

| Field | Type | Required | Description |
|---|---|---|---|
| `cursor` | `string` |  |  |
| `limit` | `integer` |  |  |
| `owner` | `string` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `object[]` | yes |  |
| `next_cursor` | `string` |  |  |

#### `jobs.read`

| Field | Type | Required | Description |
|---|---|---|---|
| `job_id` | `string` | yes |  |
| `max_bytes` | `integer` |  |  |
| `offset` | `integer` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `done` | `boolean` |  |  |
| `dropped` | `integer` |  |  |
| `next_offset` | `integer` |  |  |
| `text` | `string` | yes |  |

### llm

#### `llm.get_model`

| Field | Type | Required | Description |
|---|---|---|---|
| `ref` | `string` |  | describe this model instead (hidden or unlisted ones too); the selection is unchanged |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `available` | `boolean` | yes |  |
| `context_window` | `integer` |  |  |
| `cost` | `{ cache_read?: number, cache_write?: number, input?: number, output?: number }` |  |  |
| `default_effort` | `string` |  |  |
| `edit_tool` | `"apply_patch" \| "replace"` |  |  |
| `efforts` | `string[]` | yes |  |
| `images` | `boolean` | yes |  |
| `max_output` | `integer` |  |  |
| `model_id` | `string` | yes |  |
| `name` | `string` |  |  |
| `parameters` | `object[]` | yes |  |
| `provider` | `string` | yes |  |
| `ready` | `boolean` | yes |  |
| `ready_reason` | `string` |  |  |
| `reasoning` | `boolean` | yes |  |
| `ref` | `string` | yes |  |
| `route` | `string` |  |  |
| `route_error` | `string` |  |  |
| `tool_update` | `"addition_only"` |  |  |
| `unavailable_reason` | `string` |  |  |
| `wire_route` | `string` |  |  |

#### `llm.list_auth_keys`

| Field | Type | Required | Description |
|---|---|---|---|
| `prefix` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `string[]` | yes |  |

#### `llm.list_models`

No parameters.

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `object[]` | yes |  |

#### `llm.list_providers`

No parameters.

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `object[]` | yes |  |

#### `llm.login`

| Field | Type | Required | Description |
|---|---|---|---|
| `key` | `string` |  |  |
| `method` | `"oauth" \| "device" \| "api_key" \| "auto"` |  |  |
| `provider` | `string` | yes |  |

Result: `object`

#### `llm.logout`

| Field | Type | Required | Description |
|---|---|---|---|
| `provider` | `string` | yes |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `kept` | `string[]` | yes |  |
| `removed` | `string[]` | yes |  |
| `warnings` | `string[]` | yes |  |

#### `llm.register_provider`

| Field | Type | Required | Description |
|---|---|---|---|
| `default_model` | `string` |  |  |
| `models` | `object[]` |  |  |
| `provider` | `string` | yes |  |
| `replace` | `boolean` |  |  |
| `callbacks` | `"stream"[]` |  |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `registration_id` | `string` | yes |  |

#### `llm.set_effort`

| Field | Type | Required | Description |
|---|---|---|---|
| `also_global` | `boolean` |  |  |
| `effort` | `string` | yes |  |
| `save` | `boolean` |  |  |
| `scope_name` | `string` |  |  |
| `target` | `"session" \| "global"` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `object`

#### `llm.set_model`

| Field | Type | Required | Description |
|---|---|---|---|
| `ref` | `string` | yes |  |
| `save` | `boolean` |  |  |
| `target` | `"session" \| "global"` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `available` | `boolean` | yes |  |
| `context_window` | `integer` |  |  |
| `cost` | `{ cache_read?: number, cache_write?: number, input?: number, output?: number }` |  |  |
| `default_effort` | `string` |  |  |
| `edit_tool` | `"apply_patch" \| "replace"` |  |  |
| `efforts` | `string[]` | yes |  |
| `images` | `boolean` | yes |  |
| `max_output` | `integer` |  |  |
| `model_id` | `string` | yes |  |
| `name` | `string` |  |  |
| `parameters` | `object[]` | yes |  |
| `provider` | `string` | yes |  |
| `ready` | `boolean` | yes |  |
| `ready_reason` | `string` |  |  |
| `reasoning` | `boolean` | yes |  |
| `ref` | `string` | yes |  |
| `route` | `string` |  |  |
| `route_error` | `string` |  |  |
| `tool_update` | `"addition_only"` |  |  |
| `unavailable_reason` | `string` |  |  |
| `wire_route` | `string` |  |  |

### mcp

#### `mcp.connect`

| Field | Type | Required | Description |
|---|---|---|---|
| `server` | `string` | yes |  |

Result: `mare.McpServerStatus`

| Field | Type | Required | Description |
|---|---|---|---|
| `error` | `string` |  |  |
| `missing` | `string[]` |  |  |
| `name` | `string` | yes |  |
| `source` | `string` |  |  |
| `state` | `string` | yes |  |
| `target` | `string` |  |  |
| `tier` | `string` |  |  |
| `tools` | `integer` | yes |  |
| `transport` | `string` |  |  |

#### `mcp.get_prompt`

| Field | Type | Required | Description |
|---|---|---|---|
| `arguments` | `map&lt;string, string&gt;` |  |  |
| `name` | `string` | yes |  |
| `server` | `string` | yes |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `description` | `string` |  |  |
| `messages` | `object[]` | yes |  |

#### `mcp.list_prompts`

| Field | Type | Required | Description |
|---|---|---|---|
| `connect` | `boolean` |  |  |
| `server` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `object[]` | yes |  |

#### `mcp.list_resources`

| Field | Type | Required | Description |
|---|---|---|---|
| `connect` | `boolean` |  |  |
| `server` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `object[]` | yes |  |
| `resource_templates` | `object[]` | yes |  |

#### `mcp.list_servers`

No parameters.

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `mare.McpServerStatus[]` | yes |  |
| `errors` | `string[]` | yes |  |

#### `mcp.list_tools`

| Field | Type | Required | Description |
|---|---|---|---|
| `connect` | `boolean` |  |  |
| `server` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `object[]` | yes |  |

#### `mcp.read_resource`

| Field | Type | Required | Description |
|---|---|---|---|
| `server` | `string` | yes |  |
| `uri` | `string` | yes |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `contents` | `object[]` | yes |  |

#### `mcp.reconnect`

| Field | Type | Required | Description |
|---|---|---|---|
| `server` | `string` | yes |  |

Result: `mare.McpServerStatus`

| Field | Type | Required | Description |
|---|---|---|---|
| `error` | `string` |  |  |
| `missing` | `string[]` |  |  |
| `name` | `string` | yes |  |
| `source` | `string` |  |  |
| `state` | `string` | yes |  |
| `target` | `string` |  |  |
| `tier` | `string` |  |  |
| `tools` | `integer` | yes |  |
| `transport` | `string` |  |  |

### message

#### `message.append`

| Field | Type | Required | Description |
|---|---|---|---|
| `content` | `string \| any[]` | yes |  |
| `created_at_ms` | `integer` |  |  |
| `delivery` | `{ deliver_as?: "steer" \| "follow_up" \| "next_turn", trigger_turn?: boolean, wait?: boolean }` |  |  |
| `details` | `any` |  |  |
| `display` | `boolean` |  |  |
| `request_id` | `string` |  |  |
| `type` | `string` | yes |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `duplicate` | `boolean` |  |  |
| `message` | `object` | yes |  |
| `result` | `object` |  |  |
| `status` | `"appended" \| "queued" \| "deferred" \| "triggered"` | yes |  |

#### `message.list_types`

No parameters.

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `{ type: string }[]` | yes |  |

#### `message.register_type`

| Field | Type | Required | Description |
|---|---|---|---|
| `type` | `string` | yes |  |
| `callbacks` | `"render" \| "validate"[]` |  |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `registration_id` | `string` | yes |  |

### options

#### `options.get`

Read effective option values (matching `mare.o.<name>`). Sensitive fields (`config_sensitive`, `providers` keys/headers/query, credentials) are always redacted like `/config`.

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |
| `keys` | `string[] \| null` |  | option names or dotted paths (default: all) |
| `describe` | `boolean` |  | return per-key metadata (`effective`, `default`, `source`, `description`, `redacted`, …) instead of bare values (default `false`) |

Result: `object`

#### `options.set`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |
| `values` | `object` | yes | name → value (e.g. `{"model": "openai/gpt-6.1-terra", "effort": "high"}`) |

Result: `EmptyResult`

### peer

#### `peer.info`

No parameters.

Result: `PeerInfo`

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | `string` | yes |  |
| `pid` | `integer ≥ 0` | yes |  |
| `cwd` | `string` | yes |  |
| `version` | `string` | yes | mare version |
| `allowed` | `string[]` | yes | methods this connection may call (default set + the session's `peer.allow`) |
| `delivery_modes` | `string[]` |  | supported inbound message delivery modes (e.g. `trigger_turn`, `queue_only`) |

#### `peer.message`

`peer.message` params: a message from another session. Where the socket reports the
connecting process (Unix credentials), a `from` that disagrees with it is refused: a `pid`
other than the connecting process's, or the `name` of a registered session that isn't it.
Everything else in `from`, and `policy`, is self-reported: shown only, never grounds for
allowing anything. Always held until the receiving user confirms; a plugin can
take it over via the `peer/inbound` event. Never an approval.

| Field | Type | Required | Description |
|---|---|---|---|
| `from` | `PeerRef` |  | (default `{"name": null, "pid": null, "cwd": null, "session_id": null}`) |
| `text` | `string` | yes |  |
| `queue_only` | `boolean` |  | enqueue the message without waking an idle receiver (default `false`) |
| `policy` | `any` |  | the sender's policy as it reports it (display only until identities can be verified) |

Result: `PeerMessageResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `message_id` | `string` | yes | id of this message (repeated in `peer.message_status`) |
| `status` | `string` | yes | "held" (waiting for the receiving user; the outcome follows as `peer.message_status`) or "handled" (a plugin took it over via `peer/inbound`) |

### permissions

#### `permissions.get`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `desc` | `string` |  |  |
| `grants` | `{ network?: boolean \| string[], read?: string[], write?: string[] }` |  |  |
| `label` | `string` | yes |  |
| `level` | `string` |  |  |
| `like` | `"read-only" \| "auto" \| "full-access"` |  |  |
| `partial` | `boolean` | yes |  |
| `policy_revision` | `integer` | yes |  |
| `preset` | `string` | yes |  |
| `rules` | `object[]` |  |  |

#### `permissions.list`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `{ current: boolean, desc: string, label: string, level: string, like: "read-only" \| "auto" \| "full-access", name: string }[]` | yes |  |

#### `permissions.set`

| Field | Type | Required | Description |
|---|---|---|---|
| `confirm` | `boolean` |  |  |
| `name` | `string` | yes |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `desc` | `string` |  |  |
| `grants` | `{ network?: boolean \| string[], read?: string[], write?: string[] }` |  |  |
| `label` | `string` | yes |  |
| `level` | `string` |  |  |
| `like` | `"read-only" \| "auto" \| "full-access"` |  |  |
| `partial` | `boolean` | yes |  |
| `policy_revision` | `integer` | yes |  |
| `preset` | `string` | yes |  |
| `rules` | `object[]` |  |  |

### plugins

#### `plugins.check`

| Field | Type | Required | Description |
|---|---|---|---|
| `names` | `string[]` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `{ behind_count: integer, breaking: boolean, commits: string[], error?: string, name: string, status: "up_to_date" \| "outdated" \| "pinned" \| "local" \| "unreachable" }[]` | yes |  |

#### `plugins.install`

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | `string` |  | Optional plugin short name or declared source name; omitted operates on all eligible sources. |
| `reload` | `boolean` |  | When true and the package operation succeeds, run a full host reload. |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `log` | `string[]` | yes |  |
| `reload` | `{ added: string[], notices: string[], reloaded: string[], removed: string[], replaced: string[] }` |  |  |

#### `plugins.list`

| Field | Type | Required | Description |
|---|---|---|---|
| `kind` | `"skills" \| "prompts" \| "themes" \| "types"` |  | Optional content kind (`skills \| prompts \| themes \| types`) for `dirs`; omitted returns package roots. |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `object[]` | yes |  |
| `diagnostics` | `{ constraint?: string, message: string, path: string, plugin: string }[]` | yes |  |
| `dirs` | `{ dir: string, name: string }[]` | yes |  |

#### `plugins.uninstall`

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | `string` |  | Optional plugin short name or declared source name; omitted operates on all eligible sources. |
| `reload` | `boolean` |  | When true and the package operation succeeds, run a full host reload. |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `log` | `string[]` | yes |  |
| `reload` | `{ added: string[], notices: string[], reloaded: string[], removed: string[], replaced: string[] }` |  |  |

#### `plugins.update`

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | `string` |  | Optional plugin short name or declared source name; omitted operates on all eligible sources. |
| `reload` | `boolean` |  | When true and the package operation succeeds, run a full host reload. |
| `stash` | `boolean` |  | Save local changes and untracked files in git stash before updating. |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `log` | `string[]` | yes |  |
| `reload` | `{ added: string[], notices: string[], reloaded: string[], removed: string[], replaced: string[] }` |  |  |

### query

#### `query.call`

| Field | Type | Required | Description |
|---|---|---|---|
| `arguments` | `object` | yes |  |
| `operation` | `string` | yes |  |
| `provider` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `next_cursor` | `string` |  |  |
| `provider` | `string` |  |  |

#### `query.list_providers`

No parameters.

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `{ name: string, operations: string[], priority: integer, revision: string, source: { capability: "in_process" \| "opaque" } }[]` | yes |  |

### registrations

#### `registrations.release`

Release one registration this connection holds; answered once it is withdrawn.

| Field | Type | Required | Description |
|---|---|---|---|
| `registration_id` | `string` | yes |  |

Result: `ReleaseResult`

### services

#### `services.list`

No parameters.

Result: `ServicesListResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `ServiceInfo[]` | yes |  |

#### `services.register`

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | `string` | yes |  |
| `methods` | `map&lt;string, MethodSpec&gt;` | yes |  |
| `scope` | `RegistrationScopeKind \| null` |  |  |
| `session_id` | `string \| null` |  |  |

Result: `Registration`

| Field | Type | Required | Description |
|---|---|---|---|
| `registration_id` | `string` | yes |  |
| `name` | `string` | yes |  |

### session

#### `session.add_record`

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `any` |  |  |
| `kind` | `string` | yes |  |
| `persist` | `boolean` |  |  |
| `sync` | `boolean` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `durable` | `boolean` | yes |  |
| `record_id` | `string` | yes |  |

#### `session.archive`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |

Result: `object`

#### `session.close`

| Field | Type | Required | Description |
|---|---|---|---|
| `expected_epoch` | `string` |  |  |
| `session_id` | `string` | yes |  |

Result: `object`

#### `session.create`

| Field | Type | Required | Description |
|---|---|---|---|
| `cwd` | `string` |  |  |
| `name` | `string` |  |  |
| `request_id` | `string` |  |  |
| `subscribe` | `boolean` |  |  |

Result: `mare.LiveSession`

| Field | Type | Required | Description |
|---|---|---|---|
| `archived` | `boolean` |  | Whether the session is archived in the index. |
| `created_at_ms` | `integer` | yes | Session creation timestamp in milliseconds. |
| `cwd` | `string` | yes | Working directory of the session. |
| `ephemeral` | `boolean` |  | Whether the session is running with persistence disabled. |
| `leaf_entry_id` | `string` |  | Current leaf entry id when loaded. |
| `loaded` | `boolean` | yes | Whether this session is loaded in the current host. |
| `name` | `string` |  | Optional user-assigned or generated session name. |
| `name_revision` | `integer` |  | Monotonic revision of the session name when loaded. |
| `parent_session_id` | `string` |  | Parent or fork-origin session id, if any. |
| `path` | `string` |  | On-disk JSONL file path when saved. |
| `session_epoch` | `string` | yes | In-memory session epoch when loaded. |
| `session_id` | `string` | yes | Unique session id. |
| `status` | `"idle" \| "busy"` |  | Live activity status when loaded. |
| `updated_at_ms` | `integer` | yes | Last update timestamp in milliseconds. |

#### `session.delete`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |

Result: `object`

#### `session.fork`

| Field | Type | Required | Description |
|---|---|---|---|
| `before_item_id` | `string` |  |  |
| `expected_epoch` | `string` |  |  |
| `name` | `string` |  |  |
| `request_id` | `string` |  |  |
| `session_id` | `string` | yes |  |
| `subscribe` | `boolean` |  |  |

Result: `mare.LiveSession`

| Field | Type | Required | Description |
|---|---|---|---|
| `archived` | `boolean` |  | Whether the session is archived in the index. |
| `created_at_ms` | `integer` | yes | Session creation timestamp in milliseconds. |
| `cwd` | `string` | yes | Working directory of the session. |
| `ephemeral` | `boolean` |  | Whether the session is running with persistence disabled. |
| `leaf_entry_id` | `string` |  | Current leaf entry id when loaded. |
| `loaded` | `boolean` | yes | Whether this session is loaded in the current host. |
| `name` | `string` |  | Optional user-assigned or generated session name. |
| `name_revision` | `integer` |  | Monotonic revision of the session name when loaded. |
| `parent_session_id` | `string` |  | Parent or fork-origin session id, if any. |
| `path` | `string` |  | On-disk JSONL file path when saved. |
| `session_epoch` | `string` | yes | In-memory session epoch when loaded. |
| `session_id` | `string` | yes | Unique session id. |
| `status` | `"idle" \| "busy"` |  | Live activity status when loaded. |
| `updated_at_ms` | `integer` | yes | Last update timestamp in milliseconds. |

#### `session.get`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |

Result: `mare.Session`

| Field | Type | Required | Description |
|---|---|---|---|
| `archived` | `boolean` |  | Whether the session is archived in the index. |
| `created_at_ms` | `integer` | yes | Session creation timestamp in milliseconds. |
| `cwd` | `string` | yes | Working directory of the session. |
| `ephemeral` | `boolean` |  | Whether the session is running with persistence disabled. |
| `leaf_entry_id` | `string` |  | Current leaf entry id when loaded. |
| `loaded` | `boolean` | yes | Whether this session is loaded in the current host. |
| `name` | `string` |  | Optional user-assigned or generated session name. |
| `name_revision` | `integer` |  | Monotonic revision of the session name when loaded. |
| `parent_session_id` | `string` |  | Parent or fork-origin session id, if any. |
| `path` | `string` |  | On-disk JSONL file path when saved. |
| `session_epoch` | `string` |  | In-memory session epoch when loaded. |
| `session_id` | `string` | yes | Unique session id. |
| `source_leaf_entry_id` | `string` |  | Source leaf entry id observed when creating a snapshot, if any. |
| `status` | `"idle" \| "busy"` |  | Live activity status when loaded. |
| `updated_at_ms` | `integer` | yes | Last update timestamp in milliseconds. |

#### `session.get_item`

| Field | Type | Required | Description |
|---|---|---|---|
| `after` | `integer` |  |  |
| `before` | `integer` |  |  |
| `item_id` | `string` | yes |  |
| `max_bytes` | `integer` |  |  |
| `offset` | `integer` |  |  |
| `session_id` | `string` | yes |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `chunk` | `string` |  |  |
| `chunk_offset` | `integer` |  |  |
| `corpus_revision` | `string` | yes |  |
| `item` | `object` |  |  |
| `item_id` | `string` | yes |  |
| `neighbors` | `{ item_id: string, type: string }[]` | yes |  |
| `next_offset` | `integer` |  |  |
| `seq` | `integer` | yes |  |
| `session_id` | `string` | yes |  |
| `total_bytes` | `integer` |  |  |

#### `session.label_item`

| Field | Type | Required | Description |
|---|---|---|---|
| `item_id` | `string` |  |  |
| `label` | `string` | yes |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `item_id` | `string` | yes |  |
| `label` | `string` |  |  |

#### `session.list`

| Field | Type | Required | Description |
|---|---|---|---|
| `all` | `boolean` |  |  |
| `archived` | `boolean` |  |  |
| `cursor` | `string` |  |  |
| `cwd` | `string` |  |  |
| `limit` | `integer` |  |  |
| `loaded` | `boolean` |  |  |
| `parent_session_id` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `mare.Session[]` | yes |  |
| `next_cursor` | `string` |  |  |

#### `session.list_items`

| Field | Type | Required | Description |
|---|---|---|---|
| `branch` | `"current" \| "all"` |  |  |
| `cursor` | `string` |  |  |
| `direction` | `"asc" \| "desc"` |  |  |
| `limit` | `integer` |  |  |
| `types` | `string[]` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `object[]` | yes |  |
| `leaf_entry_id` | `string` |  |  |
| `next_cursor` | `string` |  |  |

#### `session.list_records`

| Field | Type | Required | Description |
|---|---|---|---|
| `branch` | `"current" \| "all"` |  |  |
| `cursor` | `string` |  |  |
| `kind` | `string` | yes |  |
| `limit` | `integer` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `any[]` | yes |  |
| `next_cursor` | `string` |  |  |

#### `session.load`

| Field | Type | Required | Description |
|---|---|---|---|
| `path` | `string` |  |  |
| `session_id` | `string` |  |  |
| `subscribe` | `boolean` |  |  |

Result: `mare.LiveSession`

| Field | Type | Required | Description |
|---|---|---|---|
| `archived` | `boolean` |  | Whether the session is archived in the index. |
| `created_at_ms` | `integer` | yes | Session creation timestamp in milliseconds. |
| `cwd` | `string` | yes | Working directory of the session. |
| `ephemeral` | `boolean` |  | Whether the session is running with persistence disabled. |
| `leaf_entry_id` | `string` |  | Current leaf entry id when loaded. |
| `loaded` | `boolean` | yes | Whether this session is loaded in the current host. |
| `name` | `string` |  | Optional user-assigned or generated session name. |
| `name_revision` | `integer` |  | Monotonic revision of the session name when loaded. |
| `parent_session_id` | `string` |  | Parent or fork-origin session id, if any. |
| `path` | `string` |  | On-disk JSONL file path when saved. |
| `session_epoch` | `string` | yes | In-memory session epoch when loaded. |
| `session_id` | `string` | yes | Unique session id. |
| `status` | `"idle" \| "busy"` |  | Live activity status when loaded. |
| `updated_at_ms` | `integer` | yes | Last update timestamp in milliseconds. |

#### `session.rewind`

| Field | Type | Required | Description |
|---|---|---|---|
| `before_item_id` | `string` |  |  |
| `expected_epoch` | `string` |  |  |
| `restore_files` | `boolean` |  |  |
| `session_id` | `string` | yes |  |
| `summarize` | `boolean` |  |  |

Result: `mare.LiveSession`

| Field | Type | Required | Description |
|---|---|---|---|
| `archived` | `boolean` |  | Whether the session is archived in the index. |
| `created_at_ms` | `integer` | yes | Session creation timestamp in milliseconds. |
| `cwd` | `string` | yes | Working directory of the session. |
| `ephemeral` | `boolean` |  | Whether the session is running with persistence disabled. |
| `leaf_entry_id` | `string` |  | Current leaf entry id when loaded. |
| `loaded` | `boolean` | yes | Whether this session is loaded in the current host. |
| `name` | `string` |  | Optional user-assigned or generated session name. |
| `name_revision` | `integer` |  | Monotonic revision of the session name when loaded. |
| `parent_session_id` | `string` |  | Parent or fork-origin session id, if any. |
| `path` | `string` |  | On-disk JSONL file path when saved. |
| `session_epoch` | `string` | yes | In-memory session epoch when loaded. |
| `session_id` | `string` | yes | Unique session id. |
| `status` | `"idle" \| "busy"` |  | Live activity status when loaded. |
| `updated_at_ms` | `integer` | yes | Last update timestamp in milliseconds. |

#### `session.search`

| Field | Type | Required | Description |
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

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `corpus_revision` | `string` | yes |  |
| `data` | `object[]` | yes |  |
| `incomplete` | `boolean` | yes |  |
| `next_cursor` | `string` |  |  |

#### `session.search_items`

| Field | Type | Required | Description |
|---|---|---|---|
| `cursor` | `string` |  |  |
| `event_filters` | `{ event_types?: string[], from_ms?: integer, seq_from?: integer, seq_to?: integer, surfaces?: "current" \| "shadowed" \| "log_only"[], to_ms?: integer }` |  |  |
| `filters` | `object` |  |  |
| `limit` | `integer` |  |  |
| `query` | `string` |  |  |
| `session_id` | `string` | yes |  |
| `sort_direction` | `"asc" \| "desc"` |  |  |
| `sort_key` | `"relevance" \| "created_at_ms" \| "updated_at_ms"` |  |  |
| `step_bound` | `boolean` |  |  |
| `title_query` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `corpus_revision` | `string` | yes |  |
| `data` | `object[]` | yes |  |
| `incomplete` | `boolean` | yes |  |
| `next_cursor` | `string` |  |  |

#### `session.snapshot`

| Field | Type | Required | Description |
|---|---|---|---|
| `before_item_id` | `string` |  |  |
| `effort` | `string` |  |  |
| `expected_epoch` | `string` |  |  |
| `initiating_tool_call_id` | `string` |  |  |
| `last_completed_turns` | `integer` |  |  |
| `model` | `string` |  |  |
| `name` | `string` |  |  |
| `request_id` | `string` |  |  |
| `session_id` | `string` | yes |  |

Result: `mare.Session`

| Field | Type | Required | Description |
|---|---|---|---|
| `archived` | `boolean` |  | Whether the session is archived in the index. |
| `created_at_ms` | `integer` | yes | Session creation timestamp in milliseconds. |
| `cwd` | `string` | yes | Working directory of the session. |
| `ephemeral` | `boolean` |  | Whether the session is running with persistence disabled. |
| `leaf_entry_id` | `string` |  | Current leaf entry id when loaded. |
| `loaded` | `boolean` | yes | Whether this session is loaded in the current host. |
| `name` | `string` |  | Optional user-assigned or generated session name. |
| `name_revision` | `integer` |  | Monotonic revision of the session name when loaded. |
| `parent_session_id` | `string` |  | Parent or fork-origin session id, if any. |
| `path` | `string` |  | On-disk JSONL file path when saved. |
| `session_epoch` | `string` |  | In-memory session epoch when loaded. |
| `session_id` | `string` | yes | Unique session id. |
| `source_leaf_entry_id` | `string` |  | Source leaf entry id observed when creating a snapshot, if any. |
| `status` | `"idle" \| "busy"` |  | Live activity status when loaded. |
| `updated_at_ms` | `integer` | yes | Last update timestamp in milliseconds. |

#### `session.trace`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `ancestors` | `{ created_at_ms?: integer, session_id: string, title_available: boolean }[]` | yes |  |
| `complete` | `boolean` | yes |  |
| `descendants` | `object[]` | yes |  |
| `incomplete` | `boolean` | yes |  |
| `metadata_only` | `boolean` | yes |  |
| `parent_boundary` | `boolean` | yes |  |
| `root` | `{ created_at_ms?: integer, session_id: string, title_available: boolean }` |  |  |
| `scope` | `string` | yes |  |
| `target` | `{ created_at_ms?: integer, session_id: string, title_available: boolean }` | yes |  |

#### `session.trace_item`

| Field | Type | Required | Description |
|---|---|---|---|
| `item_id` | `string` | yes |  |
| `session_id` | `string` | yes |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `availability` | `string` | yes |  |
| `created_at_ms` | `integer` |  |  |
| `derived_event_seqs` | `integer[]` | yes |  |
| `entry_id` | `string` |  |  |
| `incomplete` | `boolean` | yes |  |
| `item_id` | `string` | yes |  |
| `replaced_by` | `integer` |  |  |
| `replaced_event_seqs` | `integer[]` | yes |  |
| `replacement_chain` | `integer[]` | yes |  |
| `result_entry_id` | `string` |  |  |
| `role` | `string` |  |  |
| `seq` | `integer` | yes |  |
| `session_id` | `string` | yes |  |
| `source_event_seqs` | `integer[]` | yes |  |
| `summary` | `string` | yes |  |
| `surface` | `string` | yes |  |
| `type` | `string` | yes |  |

#### `session.unarchive`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |

Result: `object`

#### `session.update`

| Field | Type | Required | Description |
|---|---|---|---|
| `expected_revision` | `integer` |  |  |
| `name` | `string` | yes |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `mare.LiveSession`

| Field | Type | Required | Description |
|---|---|---|---|
| `archived` | `boolean` |  | Whether the session is archived in the index. |
| `created_at_ms` | `integer` | yes | Session creation timestamp in milliseconds. |
| `cwd` | `string` | yes | Working directory of the session. |
| `ephemeral` | `boolean` |  | Whether the session is running with persistence disabled. |
| `leaf_entry_id` | `string` |  | Current leaf entry id when loaded. |
| `loaded` | `boolean` | yes | Whether this session is loaded in the current host. |
| `name` | `string` |  | Optional user-assigned or generated session name. |
| `name_revision` | `integer` |  | Monotonic revision of the session name when loaded. |
| `parent_session_id` | `string` |  | Parent or fork-origin session id, if any. |
| `path` | `string` |  | On-disk JSONL file path when saved. |
| `session_epoch` | `string` | yes | In-memory session epoch when loaded. |
| `session_id` | `string` | yes | Unique session id. |
| `status` | `"idle" \| "busy"` |  | Live activity status when loaded. |
| `updated_at_ms` | `integer` | yes | Last update timestamp in milliseconds. |

### `shutdown`

No parameters.

Result: `EmptyResult`

### spill

#### `spill.delete`

| Field | Type | Required | Description |
|---|---|---|---|
| `spill_id` | `string` | yes |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `object`

#### `spill.list_backends`

No parameters.

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `{ name: string, selected: boolean }[]` | yes |  |

#### `spill.read`

| Field | Type | Required | Description |
|---|---|---|---|
| `encoding` | `"text" \| "base64"` |  |  |
| `limit` | `integer` |  |  |
| `offset` | `integer` |  |  |
| `spill_id` | `string` | yes |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `content` | `string` |  |  |
| `data_base64` | `string` |  |  |
| `next_offset` | `integer` |  |  |
| `size` | `integer` |  |  |

#### `spill.save`

| Field | Type | Required | Description |
|---|---|---|---|
| `backend` | `string` |  |  |
| `content` | `string` |  |  |
| `data_base64` | `string` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `size` | `integer` | yes |  |
| `spill_id` | `string` | yes |  |

### state

#### `state.compare_exchange`

Atomically replace durable namespace state at `expected_revision` (`ok`, `conflict` with the current snapshot, or `rejected`).
Strict JSON preserves null and empty containers; deletion requires `null` and retains a monotonic revision. Cancellation before publication prevents writes; after publication admission rollback is not guaranteed, and `unknown_state` requires reread/reconciliation.

| Field | Type | Required | Description |
|---|---|---|---|
| `namespace` | `string` | yes |  |
| `expected_revision` | `integer ≥ 0` | yes |  |
| `value` | `any` | yes |  |
| `delete` | `boolean` |  | (default `false`) |

Result: `StateCompareExchangeResult`

#### `state.snapshot`

Read isolated durable application state (distinct from `agent.get`).
Absent or deleted namespaces report `exists: false` and `null` value with tombstone revision retained.

| Field | Type | Required | Description |
|---|---|---|---|
| `namespace` | `string` | yes |  |

Result: `StateSnapshotResult`

### status

#### `status.get`

No parameters.

Result: `mare.Status`

| Field | Type | Required | Description |
|---|---|---|---|
| `cwd` | `string` | yes |  |
| `message` | `string` | yes |  |
| `model` | `string` |  |  |
| `pid` | `integer` | yes |  |
| `seq` | `integer` | yes |  |
| `session` | `{ path?: string, resume?: string[], session_id?: string }` | yes |  |
| `state` | `"idle" \| "working" \| "blocked"` | yes |  |

### subagent

#### `subagent.cancel`

| Field | Type | Required | Description |
|---|---|---|---|
| `subagent_id` | `string` | yes |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `object`

#### `subagent.get`

| Field | Type | Required | Description |
|---|---|---|---|
| `subagent_id` | `string` | yes |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `ended_at_ms` | `integer` |  |  |
| `error` | `{ code: string, data?: any, message: string }` |  |  |
| `error_kind` | `string` |  |  |
| `label` | `string` |  |  |
| `name` | `string` |  |  |
| `parent_session_id` | `string` |  |  |
| `run_id` | `string` |  |  |
| `seq` | `integer` |  |  |
| `started_at_ms` | `integer` | yes |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled" \| "interrupted"` | yes |  |
| `subagent_id` | `string` | yes | Unique identifier (child session id) of the subagent. |
| `task` | `string` |  |  |

#### `subagent.list`

| Field | Type | Required | Description |
|---|---|---|---|
| `cursor` | `string` |  |  |
| `limit` | `integer` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `object[]` | yes |  |
| `next_cursor` | `string` |  |  |

#### `subagent.list_types`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `object[]` | yes |  |

#### `subagent.register_type`

| Field | Type | Required | Description |
|---|---|---|---|
| `definition` | `object` | yes |  |
| `replace` | `boolean` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `registration_id` | `string` | yes |  |

#### `subagent.start`

| Field | Type | Required | Description |
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
| `prompt` | `string` | yes |  |
| `run_id` | `string` |  |  |
| `sections` | `object[]` |  |  |
| `seq` | `integer` |  |  |
| `system` | `string` |  |  |
| `task` | `string` |  |  |
| `tools` | `string[]` |  |  |
| `type` | `string` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `ended_at_ms` | `integer` |  |  |
| `error` | `{ code: string, data?: any, message: string }` |  |  |
| `error_kind` | `string` |  |  |
| `label` | `string` |  |  |
| `name` | `string` |  |  |
| `parent_session_id` | `string` |  |  |
| `run_id` | `string` |  |  |
| `seq` | `integer` |  |  |
| `started_at_ms` | `integer` | yes |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled" \| "interrupted"` | yes |  |
| `subagent_id` | `string` | yes | Unique identifier (child session id) of the subagent. |
| `task` | `string` |  |  |

#### `subagent.steer`

| Field | Type | Required | Description |
|---|---|---|---|
| `subagent_id` | `string` | yes |  |
| `text` | `string` | yes |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `object`

#### `subagent.wait`

| Field | Type | Required | Description |
|---|---|---|---|
| `subagent_id` | `string` | yes |  |
| `timeout_ms` | `integer` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `error` | `{ code: string, data?: any, message: string }` |  |  |
| `error_kind` | `string` |  |  |
| `messages` | `object[]` |  |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled" \| "interrupted"` | yes |  |
| `subagent_id` | `string` | yes | Unique identifier (child session id) of the subagent. |
| `text` | `string` |  |  |
| `timed_out` | `boolean` | yes |  |
| `usage` | `object` |  |  |

### terminal

#### `terminal.close`

| Field | Type | Required | Description |
|---|---|---|---|
| `grace_ms` | `integer` |  | Optional SIGTERM grace period in milliseconds (0..30000, default 3000). |
| `terminal_id` | `string` | yes | Owned terminal identifier returned by `terminal.create`. |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `mare.TerminalSessionSnapshot`

| Field | Type | Required | Description |
|---|---|---|---|
| `alternate_screen` | `boolean` |  |  |
| `cols` | `integer` | yes |  |
| `command_exit_code` | `integer` |  |  |
| `exit_code` | `integer` |  |  |
| `exit_signal` | `integer` |  |  |
| `name` | `string` |  |  |
| `output_offset` | `integer` | yes |  |
| `pid` | `integer` |  |  |
| `prompt_seq` | `integer` |  |  |
| `rows` | `integer` | yes |  |
| `screen` | `string` |  |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled"` | yes |  |
| `terminal_id` | `string` | yes |  |
| `truncated` | `boolean` | yes |  |
| `view_truncated` | `boolean` |  |  |

#### `terminal.create`

| Field | Type | Required | Description |
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
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `mare.TerminalSessionSnapshot`

| Field | Type | Required | Description |
|---|---|---|---|
| `alternate_screen` | `boolean` |  |  |
| `cols` | `integer` | yes |  |
| `command_exit_code` | `integer` |  |  |
| `exit_code` | `integer` |  |  |
| `exit_signal` | `integer` |  |  |
| `name` | `string` |  |  |
| `output_offset` | `integer` | yes |  |
| `pid` | `integer` |  |  |
| `prompt_seq` | `integer` |  |  |
| `rows` | `integer` | yes |  |
| `screen` | `string` |  |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled"` | yes |  |
| `terminal_id` | `string` | yes |  |
| `truncated` | `boolean` | yes |  |
| `view_truncated` | `boolean` |  |  |

#### `terminal.list`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `mare.TerminalSessionSnapshot[]` | yes |  |

#### `terminal.read`

| Field | Type | Required | Description |
|---|---|---|---|
| `count` | `integer` |  |  |
| `max_bytes` | `integer` |  | Maximum output bytes to return (1..4194304, default 262144). Applies to both incremental `output_offset` slicing (UTF-8 safe, floor one char) and VT page mode as well as `screen`; sets `truncated` true when capped. |
| `offset` | `integer` |  |  |
| `output_offset` | `integer` |  |  |
| `screen` | `boolean` |  |  |
| `terminal_id` | `string` | yes | Owned terminal identifier returned by `terminal.create`. |
| `wait_ms` | `integer` |  | Optional bounded wait in milliseconds (0..60000). |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `alternate_screen` | `boolean` |  |  |
| `cols` | `integer` | yes |  |
| `command_exit_code` | `integer` |  |  |
| `exit_code` | `integer` |  |  |
| `exit_signal` | `integer` |  |  |
| `line_begin` | `integer` | yes |  |
| `line_end` | `integer` | yes |  |
| `name` | `string` |  |  |
| `next_output_offset` | `integer` | yes |  |
| `output_offset` | `integer` | yes |  |
| `pid` | `integer` |  |  |
| `prompt_seq` | `integer` |  |  |
| `rows` | `integer` | yes |  |
| `screen` | `string` |  |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled"` | yes |  |
| `terminal_id` | `string` | yes |  |
| `text` | `string` | yes |  |
| `total_lines` | `integer` | yes |  |
| `truncated` | `boolean` | yes |  |
| `view_truncated` | `boolean` |  |  |

#### `terminal.resize`

| Field | Type | Required | Description |
|---|---|---|---|
| `cols` | `integer` | yes | Terminal columns (1..500, default 80). |
| `rows` | `integer` | yes | Terminal rows (1..200, default 24). |
| `terminal_id` | `string` | yes | Owned terminal identifier returned by `terminal.create`. |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `object`

#### `terminal.send`

| Field | Type | Required | Description |
|---|---|---|---|
| `max_bytes` | `integer` |  | Maximum output bytes to return (1..4194304, default 262144). Output is truncated at a UTF-8 boundary (always returning at least one character if non-empty) and sets `truncated` true; when `screen` is true, rendered screen is head-trimmed to fit and sets `view_truncated`. |
| `screen` | `boolean` |  |  |
| `submit` | `boolean` |  |  |
| `terminal_id` | `string` | yes | Owned terminal identifier returned by `terminal.create`. |
| `text` | `string` |  |  |
| `wait_ms` | `integer` |  | Optional bounded wait in milliseconds (0..60000). |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `alternate_screen` | `boolean` |  |  |
| `cols` | `integer` | yes |  |
| `command_exit_code` | `integer` |  |  |
| `exit_code` | `integer` |  |  |
| `exit_signal` | `integer` |  |  |
| `name` | `string` |  |  |
| `next_output_offset` | `integer` | yes |  |
| `output` | `string` | yes |  |
| `output_offset` | `integer` | yes |  |
| `pid` | `integer` |  |  |
| `prompt_seq` | `integer` |  |  |
| `rows` | `integer` | yes |  |
| `screen` | `string` |  |  |
| `status` | `"running" \| "completed" \| "failed" \| "cancelled"` | yes |  |
| `terminal_id` | `string` | yes |  |
| `timed_out` | `boolean` | yes |  |
| `truncated` | `boolean` | yes |  |
| `view_truncated` | `boolean` |  |  |

#### `terminal.signal`

| Field | Type | Required | Description |
|---|---|---|---|
| `signal` | `"SIGINT" \| "SIGTERM" \| "SIGKILL" \| "SIGTSTP" \| "SIGHUP"` | yes |  |
| `terminal_id` | `string` | yes | Owned terminal identifier returned by `terminal.create`. |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `object`

### theme

#### `theme.cancel_preview`

No parameters.

Result: `mare.ThemeSnapshot`

| Field | Type | Required | Description |
|---|---|---|---|
| `active` | `string` | yes |  |
| `appearance` | `"dark" \| "light"` | yes |  |
| `component` | `any` | yes |  |
| `dark` | `string` | yes |  |
| `icons` | `map&lt;string, string&gt;` | yes |  |
| `light` | `string` | yes |  |
| `preference` | `string` | yes |  |
| `preview` | `string` |  |  |
| `revision` | `integer` | yes |  |
| `styles` | `map&lt;string, object&gt;` | yes |  |
| `syntax` | `string` |  |  |

#### `theme.get`

No parameters.

Result: `mare.ThemeSnapshot`

| Field | Type | Required | Description |
|---|---|---|---|
| `active` | `string` | yes |  |
| `appearance` | `"dark" \| "light"` | yes |  |
| `component` | `any` | yes |  |
| `dark` | `string` | yes |  |
| `icons` | `map&lt;string, string&gt;` | yes |  |
| `light` | `string` | yes |  |
| `preference` | `string` | yes |  |
| `preview` | `string` |  |  |
| `revision` | `integer` | yes |  |
| `styles` | `map&lt;string, object&gt;` | yes |  |
| `syntax` | `string` |  |  |

#### `theme.list`

No parameters.

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `{ label: string, name: string, origin: string, variants: "dark" \| "light"[] }[]` | yes |  |

#### `theme.preview`

| Field | Type | Required | Description |
|---|---|---|---|
| `theme` | `string` | yes |  |

Result: `mare.ThemeSnapshot`

| Field | Type | Required | Description |
|---|---|---|---|
| `active` | `string` | yes |  |
| `appearance` | `"dark" \| "light"` | yes |  |
| `component` | `any` | yes |  |
| `dark` | `string` | yes |  |
| `icons` | `map&lt;string, string&gt;` | yes |  |
| `light` | `string` | yes |  |
| `preference` | `string` | yes |  |
| `preview` | `string` |  |  |
| `revision` | `integer` | yes |  |
| `styles` | `map&lt;string, object&gt;` | yes |  |
| `syntax` | `string` |  |  |

#### `theme.register`

| Field | Type | Required | Description |
|---|---|---|---|
| `definition` | `object` | yes |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `registration_id` | `string` | yes |  |

#### `theme.register_override`

| Field | Type | Required | Description |
|---|---|---|---|
| `layer_id` | `string` | yes |  |
| `styles` | `map&lt;string, object&gt;` | yes |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `registration_id` | `string` | yes |  |

#### `theme.select`

| Field | Type | Required | Description |
|---|---|---|---|
| `save` | `boolean` |  |  |
| `theme` | `string` | yes |  |

Result: `mare.ThemeSnapshot`

| Field | Type | Required | Description |
|---|---|---|---|
| `active` | `string` | yes |  |
| `appearance` | `"dark" \| "light"` | yes |  |
| `component` | `any` | yes |  |
| `dark` | `string` | yes |  |
| `icons` | `map&lt;string, string&gt;` | yes |  |
| `light` | `string` | yes |  |
| `preference` | `string` | yes |  |
| `preview` | `string` |  |  |
| `revision` | `integer` | yes |  |
| `styles` | `map&lt;string, object&gt;` | yes |  |
| `syntax` | `string` |  |  |

### tools

#### `tools.register`

| Field | Type | Required | Description |
|---|---|---|---|
| `access` | `{ kind: "file", op: "read" \| "write" \| "create" \| "delete", path_arg: string, recursive: boolean }[]` |  |  |
| `concurrency_safe` | `boolean` |  |  |
| `description` | `string` | yes |  |
| `exposure` | `"direct" \| "deferred" \| "nested"` |  |  |
| `guidelines` | `string[]` |  |  |
| `idempotent` | `boolean` |  |  |
| `name` | `string` | yes |  |
| `parameters` | `object` | yes |  |
| `side_effect` | `"read" \| "write" \| "external"` |  |  |
| `snippet` | `string` |  |  |
| `timeout_ms` | `integer` |  |  |
| `callbacks` | `"execute"[]` | yes |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `registration_id` | `string` | yes |  |

### ui

#### `ui.attach`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |
| `responder` | `boolean \| null` |  |  |
| `takeover` | `string[]` |  | (default `[]`) |
| `viewport` | `UiViewport \| null` |  |  |

Result: `UiAttachResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |
| `session_epoch` | `string` | yes |  |
| `responder` | `boolean` | yes |  |
| `takeover` | `string[]` | yes |  |
| `snapshot` | `UiShellSnapshot` | yes |  |

#### `ui.call_keymap`

| Field | Type | Required | Description |
|---|---|---|---|
| `action` | `string` |  |  |
| `context` | `string` |  |  |
| `key` | `string` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `action` | `string` |  |  |
| `context` | `string` |  |  |
| `declined` | `boolean` | yes |  |
| `handled` | `boolean` | yes |  |
| `masked` | `boolean` | yes |  |
| `reason` | `string` |  |  |

#### `ui.cancel_markdown`

| Field | Type | Required | Description |
|---|---|---|---|
| `document_id` | `string` | yes |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `object`

#### `ui.delete_keymap`

| Field | Type | Required | Description |
|---|---|---|---|
| `context` | `string` | yes |  |
| `key` | `string` | yes |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `context` | `string` | yes |  |
| `key` | `string` | yes |  |

#### `ui.delete_markdown`

| Field | Type | Required | Description |
|---|---|---|---|
| `document_id` | `string` | yes |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `object`

#### `ui.detach`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |

Result: `UiDetachResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |

#### `ui.get_keymap`

| Field | Type | Required | Description |
|---|---|---|---|
| `context` | `string` |  | Optional context name to filter by. |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `actions` | `{ contexts?: string[], core?: boolean, desc?: string, essential?: boolean, keys?: string[], name: string }[]` | yes |  |
| `bindings` | `object[]` | yes |  |
| `conflicts` | `{ missing_essential: { action: string, context: string }[], unbound_essential: { action: string, context: string }[] }` | yes |  |
| `contexts` | `{ bindable: boolean, chain: string[], core: boolean, name: string, parents: string[] }[]` | yes |  |

#### `ui.get_markdown`

| Field | Type | Required | Description |
|---|---|---|---|
| `document_id` | `string` | yes |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `mare.MarkdownSnapshot`

| Field | Type | Required | Description |
|---|---|---|---|
| `diagnostic` | `string` |  |  |
| `document_id` | `string` | yes |  |
| `enhanced` | `boolean` | yes |  |
| `expanded_node` | `mare.Node` | yes |  |
| `node` | `mare.Node` | yes |  |
| `pending` | `integer` | yes |  |
| `render_seq` | `integer` | yes |  |
| `revision` | `integer` | yes |  |
| `session_epoch` | `string` |  |  |
| `session_id` | `string` |  |  |
| `source` | `string` | yes |  |
| `supports_artifact_images` | `boolean` | yes |  |

#### `ui.get_select_preview`

| Field | Type | Required | Description |
|---|---|---|---|
| `index` | `integer` | yes |  |
| `select_id` | `string` | yes |  |
| `tab` | `integer` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `preview` | `string \| mare.Node` |  |  |

#### `ui.invalidate`

| Field | Type | Required | Description |
|---|---|---|---|
| `component` | `string` |  | Widget (`widget:<id>` or widget id) or status segment id to redraw. |
| `viewport` | `{ height?: integer, supports_artifact_images?: boolean, supports_images?: boolean, width?: integer }` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `object`

#### `ui.invalidate_presentations`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `object`

#### `ui.list_markdown_presenters`

No parameters.

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `{ language: string, preview: boolean, requires_artifact_images: boolean, timeout_ms: integer }[]` | yes |  |

#### `ui.register_entry_renderer`

| Field | Type | Required | Description |
|---|---|---|---|
| `kind` | `string` | yes |  |
| `callbacks` | `"render"[]` | yes |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `registration_id` | `string` | yes |  |

#### `ui.register_markdown_transform`

| Field | Type | Required | Description |
|---|---|---|---|
| `message_types` | `"user" \| "assistant" \| "assistant-reasoning" \| "custom" \| "summary" \| "changelog"[]` |  |  |
| `callbacks` | `"transform"[]` | yes |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `registration_id` | `string` | yes |  |

#### `ui.render_markdown`

| Field | Type | Required | Description |
|---|---|---|---|
| `base_node` | `mare.Node` |  |  |
| `document_id` | `string` | yes |  |
| `replay` | `boolean` |  |  |
| `source` | `string` | yes |  |
| `viewport` | `{ height?: integer, supports_artifact_images?: boolean, supports_images?: boolean, width?: integer }` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result: `mare.MarkdownSnapshot`

| Field | Type | Required | Description |
|---|---|---|---|
| `diagnostic` | `string` |  |  |
| `document_id` | `string` | yes |  |
| `enhanced` | `boolean` | yes |  |
| `expanded_node` | `mare.Node` | yes |  |
| `node` | `mare.Node` | yes |  |
| `pending` | `integer` | yes |  |
| `render_seq` | `integer` | yes |  |
| `revision` | `integer` | yes |  |
| `session_epoch` | `string` |  |  |
| `session_id` | `string` |  |  |
| `source` | `string` | yes |  |
| `supports_artifact_images` | `boolean` | yes |  |

#### `ui.send_editor`

| Field | Type | Required | Description |
|---|---|---|---|
| `attachments` | `object[]` |  |  |
| `mode` | `"prompt" \| "steer" \| "follow_up"` |  |  |
| `text` | `string` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `handled` | `boolean` | yes |  |
| `result` | `object` |  |  |
| `revision` | `integer` | yes |  |

#### `ui.set_external_editor`

| Field | Type | Required | Description |
|---|---|---|---|
| `action` | `"start" \| "commit" \| "cancel"` | yes |  |
| `text` | `string` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `program` | `string` |  |  |
| `revision` | `integer` | yes |  |
| `running` | `boolean` | yes |  |
| `text` | `string` | yes |  |

#### `ui.set_keymap`

| Field | Type | Required | Description |
|---|---|---|---|
| `action` | `string \| boolean` | yes |  |
| `context` | `string` | yes |  |
| `desc` | `string` |  |  |
| `key` | `string` | yes |  |
| `priority` | `number` |  |  |
| `unique` | `boolean` |  |  |
| `when` | `string[]` |  |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `registration_id` | `string` | yes |  |

#### `ui.transform_markdown`

| Field | Type | Required | Description |
|---|---|---|---|
| `available_width` | `integer` |  |  |
| `created_at_ms` | `integer` |  |  |
| `is_streaming` | `boolean` |  |  |
| `message_id` | `string` |  |  |
| `message_type` | `"user" \| "assistant" \| "assistant-reasoning" \| "custom" \| "summary" \| "changelog"` |  |  |
| `text` | `string` | yes |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `text` | `string` | yes |  |

#### `ui.update_editor`

| Field | Type | Required | Description |
|---|---|---|---|
| `attachments` | `{ attachment_id: string, height?: integer, label?: string, mime_type?: string, width?: integer }[]` |  |  |
| `image_markers` | `string[]` |  |  |
| `offset` | `integer` |  |  |
| `shell_mode` | `boolean` |  |  |
| `text` | `string` | yes |  |
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `attachments` | `{ attachment_id: string, height?: integer, label?: string, mime_type?: string, width?: integer }[]` |  |  |
| `external_editor_running` | `boolean` | yes |  |
| `image_markers` | `string[]` |  |  |
| `offset` | `integer` | yes |  |
| `revision` | `integer` | yes |  |
| `shell_mode` | `boolean` | yes |  |
| `text` | `string` | yes |  |

### usage

#### `usage.get`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |
| `expected_epoch` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `context_tokens` | `integer` | yes |  |
| `rate_limits` | `map&lt;string, { credits?: string, limit_name?: string, primary?: object, reached?: string, received_at_ms: integer, secondary?: object }&gt;` | yes |  |
| `totals` | `object` | yes |  |

### web

#### `web.fetch`

| Field | Type | Required | Description |
|---|---|---|---|
| `timeout_ms` | `integer` |  |  |
| `url` | `string` | yes |  |

Result: `mare.WebFetchResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `content` | `string` | yes |  |
| `content_type` | `string` |  |  |
| `requested_url` | `string` | yes |  |
| `status` | `integer` |  |  |
| `truncated` | `boolean` |  |  |
| `url` | `string` |  |  |

#### `web.list_providers`

| Field | Type | Required | Description |
|---|---|---|---|
| `kind` | `"search" \| "fetch"` | yes |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `{ available: boolean, provider_id: string, selected: boolean, unconfined: boolean }[]` | yes |  |
| `reason` | `string` |  |  |

#### `web.register_fetch`

| Field | Type | Required | Description |
|---|---|---|---|
| `provider_id` | `string` | yes |  |
| `unconfined` | `boolean` |  |  |
| `callbacks` | `"fetch"[]` | yes |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `registration_id` | `string` | yes |  |

#### `web.register_search`

| Field | Type | Required | Description |
|---|---|---|---|
| `provider_id` | `string` | yes |  |
| `unconfined` | `boolean` |  |  |
| `callbacks` | `"search"[]` | yes |  |
| `scope` | `"all" \| "session"` |  |  |
| `session_id` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `registration_id` | `string` | yes |  |

#### `web.search`

| Field | Type | Required | Description |
|---|---|---|---|
| `queries` | `string[]` | yes |  |
| `timeout_ms` | `integer` |  |  |

Result: `mare.WebSearchResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `content` | `string` |  |  |
| `data` | `{ published_at_ms?: integer, snippet?: string, summary?: string, title?: string, url: string }[]` | yes |  |
| `truncated` | `boolean` |  |  |

### webserver

#### `webserver.get`

No parameters.

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `host` | `string` |  |  |
| `port` | `integer` |  |  |
| `running` | `boolean` | yes |  |
| `tls` | `boolean` |  |  |
| `trusted_hosts` | `string[]` |  |  |
| `url` | `string` |  |  |
| `ws_url` | `string` |  |  |

#### `webserver.list_assets`

No parameters.

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `{ asset_id: string, dir: string, fallback?: string, index?: string, plugin?: string, prefix: string }[]` | yes |  |

#### `webserver.list_routes`

No parameters.

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `data` | `{ method: string, owner_session_id?: string, path: string, plugin?: string, route_id: string }[]` | yes |  |

#### `webserver.register_assets`

| Field | Type | Required | Description |
|---|---|---|---|
| `dir` | `string` | yes |  |
| `fallback` | `string` |  |  |
| `index` | `string` |  |  |
| `prefix` | `string` |  |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `registration_id` | `string` | yes |  |

#### `webserver.register_route`

| Field | Type | Required | Description |
|---|---|---|---|
| `method` | `"GET" \| "POST" \| "PUT" \| "PATCH" \| "DELETE" \| "HEAD" \| "ANY"` |  |  |
| `path` | `string` | yes |  |
| `callbacks` | `"handle"[]` | yes |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `registration_id` | `string` | yes |  |

### workspace

#### `workspace.get`

| Field | Type | Required | Description |
|---|---|---|---|
| `cwd` | `string` | yes |  |

Result:

| Field | Type | Required | Description |
|---|---|---|---|
| `dirs` | `string[]` | yes |  |
| `root` | `string` | yes |  |
| `trusted` | `boolean` | yes |  |

#### `workspace.set_trust`

| Field | Type | Required | Description |
|---|---|---|---|
| `save` | `boolean` |  |  |
| `trusted` | `boolean` | yes |  |
| `cwd` | `string` | yes |  |

Result: `object`

## Notifications (mare → client)

### `$/cancel_request`

Cancels an outstanding mare-to-host request. Late replies to this ID are ignored.

| Field | Type | Required | Description |
|---|---|---|---|
| `id` | `RequestId` | yes |  |

### `dropped`

Drop count for unretained `lossy` or refused events omitted from external stream consumers.

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `session_epoch` | `string \| null` |  |  |
| `count` | `integer ≥ 0` | yes |  |

### `event`

Envelope of an `event` notification: unified across RPC and headless streams.

| Field | Type | Required | Description |
|---|---|---|---|
| `type` | `string` | yes | kernel event name, e.g. `turn/started`, `item/delta`, `item/completed`, `turn/completed` |
| `session_id` | `string \| null` |  | session the event belongs to |
| `session_epoch` | `string \| null` |  | opaque in-memory epoch identifier that rotates on every session create, load, or switch |
| `seq` | `integer ≥ 0` |  | per-session, per-epoch sequence number (1-based, contiguous) (default `0`) |
| `turn_id` | `string \| null` |  | run (prompt) the event belongs to — matches `agent.prompt` `turn_id` |
| `ignorable` | `boolean \| null` |  | whether a client that does not recognize this event type may skip it |
| `data` | `any` |  |  |

### `fs/changed`

| Field | Type | Required | Description |
|---|---|---|---|
| `watch_id` | `string` | yes |  |
| `changed_paths` | `string[]` | yes |  |
| `truncated` | `boolean` |  |  |

### `lagged`

Sequence range omitted when an ordinary consumer cursor fell behind the bounded event log.

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `session_epoch` | `string \| null` |  |  |
| `from_seq` | `integer ≥ 0` | yes |  |
| `to_seq` | `integer ≥ 0` | yes |  |

### `peer.message_status`

`peer.message_status` notification (mare → peer): the receiving user's decision on a held
message.

| Field | Type | Required | Description |
|---|---|---|---|
| `message_id` | `string` | yes |  |
| `status` | `string` | yes | "delivered" \| "rejected" |
| `reason` | `string \| null` |  |  |

### `snapshot`

`snapshot` notification (mare → host): emitted immediately after an `events.subscribe` reply
with `mode = "baseline"`, and immediately after `lagged` when a live cursor is evicted.

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `session_epoch` | `string` | yes |  |
| `seq` | `integer ≥ 0` | yes | Ring tail sequence number `S` at the moment of the non-yielding cut; following events start at `S + 1`. |
| `baseline_id` | `string \| null` |  | Opaque handle for paging `parts` via `baseline.read`. |
| `retained` | `object` |  | Retained state (`events`, `status`, `queue`, `active_turn`, `active_items` skeletons, pending approvals/interactions). (default `{}`) |
| `parts` | `string[]` |  | Baseline parts available to page via `baseline.read` (e.g. `["entries", "active_items"]`). (default `[]`) |

### `stream.failed`

`event` notification params — exactly one `mare -p --json` line.
An event stream is incomplete and this connection will close.

| Field | Type | Required | Description |
|---|---|---|---|
| `error` | `string` | yes |  |

### `ui.editor_config`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `prompt` | `string \| null` |  |  |
| `placeholder` | `string \| null` |  |  |
| `busy_placeholder` | `string \| null` |  |  |
| `border` | `string \| null` |  |  |

### `ui.editor_hint`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `text` | `string` | yes |  |

### `ui.editor_state`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `text` | `string` | yes |  |
| `cursor` | `integer ≥ 0` | yes |  |
| `shell_mode` | `boolean` |  | (default `false`) |
| `image_markers` | `string[]` |  |  |
| `attachments` | `UiEditorAttachment[]` |  |  |
| `external_editor_running` | `boolean` |  | (default `false`) |
| `revision` | `integer ≥ 0` | yes |  |

### `ui.markdown_changed`

| Field | Type | Required | Description |
|---|---|---|---|
| `document_id` | `string` | yes |  |
| `source` | `string` | yes |  |
| `revision` | `integer ≥ 0` | yes |  |
| `render_seq` | `integer ≥ 0` | yes |  |
| `session_id` | `string \| null` |  |  |
| `session_epoch` | `string \| null` |  |  |
| `pending` | `integer ≥ 0` | yes |  |
| `enhanced` | `boolean` | yes |  |
| `supports_artifact_images` | `boolean` | yes |  |
| `diagnostic` | `string \| null` |  |  |
| `node` | `any` | yes | Existing Node description. Image bytes become bounded canonical base64 `data`; no paths, callbacks, tokens, or userdata. Deserialize via mare.ui.node_from_wire in a Lua frontend. |
| `expanded_node` | `any` | yes | Same Markdown document with each enhanced fence followed by its complete source. |

### `ui.notify`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `text` | `string` | yes |  |
| `level` | `string \| null` |  | info \| success \| warning \| error |

### `ui.overlay`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `title` | `string` | yes |  |
| `node` | `any` |  |  |
| `open` | `boolean` | yes |  |
| `context` | `string \| null` |  |  |

### `ui.set_editor`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `text` | `string` | yes |  |
| `keep_draft` | `boolean` |  | (default `false`) |
| `prepend` | `boolean` |  | (default `false`) |

### `ui.status`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `status_id` | `string` | yes |  |
| `align` | `string` | yes |  |
| `order` | `integer` |  | (default `0`) |
| `keep` | `boolean` |  | (default `false`) |
| `node` | `any` |  |  |

### `ui.widget`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `widget_id` | `string` | yes |  |
| `slot` | `string` | yes |  |
| `order` | `integer` |  | (default `0`) |
| `node` | `any` |  |  |

### `ui/component-updated`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes |  |
| `component` | `string` | yes |  |
| `payload` | `any` |  |  |

### `ui/select-closed`

| Field | Type | Required | Description |
|---|---|---|---|
| `select_id` | `string` | yes |  |

### `ui/select-updated`

| Field | Type | Required | Description |
|---|---|---|---|
| `select_id` | `string` | yes |  |
| `items` | `UiSelectItem[]` |  |  |
| `tabs` | `UiSelectTab[]` |  |  |

## Notifications (client → mare)

### `$/cancel_request`

Cancels an outstanding mare-to-host request. Late replies to this ID are ignored.

| Field | Type | Required | Description |
|---|---|---|---|
| `id` | `RequestId` | yes |  |

### `callback.update`

`callback.update`: client → server notification streaming a partial update for an in-flight
`callback.call` request identified by `id`.

| Field | Type | Required | Description |
|---|---|---|---|
| `id` | `RequestId` | yes |  |
| `partial` | `any` | yes |  |

## Callbacks (mare → client requests)

### `approval.request`

`approval.request` (mare → host): a tool call or a network host needs approval.
No answer (timeout, disconnect, error, invalid result) counts as `deny`.

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes | session that needs approval |
| `turn_id` | `string \| null` |  | active turn id, when known |
| `kind` | `string` | yes | "tool" or "network" |
| `approval_id` | `string \| null` |  | unique approval lifecycle id (pairs with `approval/requested` and `approval/resolved`) |
| `tool_call_id` | `string \| null` |  | tool call id (kind = tool) |
| `tool` | `string \| null` |  | tool name (kind = tool) |
| `arguments` | `any` |  | the call's arguments (kind = tool) |
| `command` | `string \| null` |  | shell command, when the tool runs one |
| `diff` | `string \| null` |  | proposed change (patch / edit), when the tool writes files |
| `host` | `string \| null` |  | host:port (kind = network) |
| `cwd` | `string` | yes |  |
| `reason` | `string \| null` |  | why approval is needed (from the policy vote) |
| `side_effect` | `string \| null` |  | "read" \| "write" \| "external" |
| `unconfined` | `boolean` |  | runs outside the local sandbox (MCP / RPC-host tools) (default `false`) |
| `proposed_amendment` | `any` |  | a rule the policy proposes for `allow_amend` (opaque to core) |
| `requested_permissions` | `PermissionProfile \| null` |  | Explicit permission increment requested by this tool call (read/write paths, network hosts). |
| `scope` | `PermissionGrantScope \| null` |  | Requested maximum lifetime, when this is a structured permission request. |

Result: `ApprovalRequestResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `decision` | `ApprovalDecision` | yes |  |
| `permissions` | `PermissionProfile` |  | Optional structured subset of the requested read/write/network increment. |
| `scope` | `PermissionGrantScope` |  | Lifetime of the grant: turn or session (the request remains an upper bound). |
| `amendment` | `any` |  | with `allow_amend`: the rule to persist (opaque to core) |
| `reason` | `string \| null` |  | shown to the model when denied |

### `callback.call`

`callback.call`: server → client request invoking one contract callback on a registration the
client holds. Arguments match the callback's params schema; result is
checked against its result schema.

| Field | Type | Required | Description |
|---|---|---|---|
| `registration_id` | `string` | yes |  |
| `callback` | `string` | yes |  |
| `session_id` | `string \| null` |  |  |
| `turn_id` | `string \| null` |  |  |
| `tool_call_id` | `string \| null` |  |  |
| `caller_id` | `string \| null` |  |  |
| `arguments` | `any` | yes |  |

Result: `any`

### `mcp.elicitation`

`mcp.elicitation` (mare → host): an MCP server requested user input or URL confirmation.

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string` | yes | session that requested elicitation |
| `turn_id` | `string \| null` |  | active turn id, when known |
| `server` | `string` | yes |  |
| `request_id` | `any` |  |  |
| `mode` | `string \| null` |  |  |
| `message` | `string` |  | (default `""`) |
| `requested_schema` | `any` |  |  |
| `url` | `string \| null` |  |  |
| `elicitation_id` | `string \| null` |  |  |
| `_meta` | `any` |  |  |

Result: `McpElicitationResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `action` | `McpElicitationAction` | yes |  |
| `content` | `any` |  |  |

### `ui.confirm`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `prompt` | `string` | yes |  |
| `default` | `boolean \| null` |  |  |

Result: `UiConfirmResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `confirmed` | `boolean \| null` |  | true/false when answered; null when cancelled or interrupted |
| `reason` | `string \| null` |  | Dismissal reason when cancelled (`"cancelled"` \| `"interrupted"`) |

### `ui.editor`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `title` | `string` |  | (default `""`) |
| `prefill` | `string` |  | (default `""`) |

Result: `UiTextPromptResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `text` | `string \| null` |  | null when cancelled |
| `reason` | `string \| null` |  | Dismissal reason when cancelled (`"cancelled"` \| `"interrupted"`) |

### `ui.input`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `title` | `string` |  | (default `""`) |
| `placeholder` | `string` |  | (default `""`) |
| `default` | `string \| null` |  |  |
| `secret` | `boolean` |  | (default `false`) |

Result: `UiTextPromptResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `text` | `string \| null` |  | null when cancelled |
| `reason` | `string \| null` |  | Dismissal reason when cancelled (`"cancelled"` \| `"interrupted"`) |

### `ui.questions`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `title` | `string \| null` |  |  |
| `questions` | `UiQuestionItem[]` | yes |  |

Result: `UiQuestionsResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `answers` | `map&lt;string, UiQuestionAnswer&gt;` |  | (default `{}`) |
| `dismissed` | `boolean \| null` |  |  |
| `reason` | `string \| null` |  |  |
| `note` | `string \| null` |  |  |

### `ui.select`

| Field | Type | Required | Description |
|---|---|---|---|
| `session_id` | `string \| null` |  |  |
| `select_id` | `string \| null` |  |  |
| `title` | `string` |  | (default `""`) |
| `title_right` | `any` |  |  |
| `body` | `any[]` |  | Explanatory lines shown above the options (strings or wire `Node` objects). |
| `items` | `UiSelectItem[]` | yes |  |
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

Result: `UiSelectResult`

| Field | Type | Required | Description |
|---|---|---|---|
| `index` | `integer \| null` |  |  |
| `selected_indices` | `integer ≥ 0[] \| null` |  | 0-based checked item indices for a multi-select; an empty array is a valid answer. |
| `tab` | `integer \| null` |  |  |
| `key` | `string \| null` |  |  |
| `filter` | `string \| null` |  |  |
| `reason` | `string \| null` |  | Dismissal reason when cancelled (`"cancelled"` \| `"interrupted"`) |

## Error codes

An error's `data.code` says what failed and `data.data` carries its additional data. A service's own codes are `<service>.<name>`, declared by its contract (`services.list` shows them); anything else is `internal`.

| `data.code` | number | meaning |
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
