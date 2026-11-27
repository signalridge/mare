# イベントリファレンス

組み込みの公開プロトコルイベントとコントラクトイベント、およびそのペイロードスキーマです。使い方は [JSON イベントストリーム](../integrate/json-stream.md) と [サービスとイベント](../extend/services-and-events.md) を参照してください。

> このページは `scripts/gen_docs.py` がコードから生成します。手で編集しないでください。説明文はコード由来のため英語のままです。

プロトコルバージョン: **3.0**

## turn

### `turn/started`

Agent turn started.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `required` | いいえ | いいえ | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `turn_id` | `string` | はい |  |
| `trigger` | `string` | はい |  |
| `input_item_ids` | `string[] \| null` |  |  |
| `text` | `string \| null` |  |  |
| `source` | `string \| null` |  |  |
| `truncated` | `boolean \| null` |  |  |

**例**

```json
{
  "turn_id": "t-1",
  "trigger": "prompt"
}
```

### `turn/completed`

Agent turn completed after run-end durability confirmation.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `required` | いいえ | いいえ | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `turn_id` | `string` | はい |  |
| `status` | `TurnStatus` | はい |  |
| `termination_reason` | `AgentTerminationReason \| null` |  |  |
| `cancellation_cause` | `CancellationCause \| null` |  | Origin of cancellation when `termination_reason` is `interrupted`. |
| `text` | `string \| null` |  |  |
| `structured` | `any` |  |  |
| `usage` | `object \| null` |  |  |
| `error` | `EventError \| null` |  |  |
| `error_kind` | `string \| null` |  |  |
| `not_sent` | `boolean \| null` |  |  |
| `truncated` | `boolean \| null` |  |  |

**例**

```json
{
  "turn_id": "t-1",
  "status": "completed",
  "usage": {}
}
```

## step

### `step/started`

Model step started within a turn.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `required` | いいえ | いいえ | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `turn_id` | `string` | はい |  |
| `step_id` | `string` | はい |  |
| `turn` | `integer \| null` |  |  |
| `model` | `any` |  |  |
| `attempt` | `integer \| null` |  |  |
| `truncated` | `boolean \| null` |  |  |

**例**

```json
{
  "turn_id": "t-1",
  "step_id": "s-1",
  "turn": 1,
  "attempt": 0
}
```

### `step/completed`

Model step finished (completed, failed, or retrying).

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `required` | いいえ | いいえ | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `turn_id` | `string` | はい |  |
| `step_id` | `string` | はい |  |
| `status` | `StepStatus` | はい |  |
| `stop_reason` | `string \| null` |  |  |
| `usage` | `any` |  |  |
| `error` | `EventError \| null` |  |  |
| `error_kind` | `string \| null` |  |  |
| `will_retry` | `boolean \| null` |  |  |
| `attempt` | `integer \| null` |  |  |
| `delay_ms` | `number \| null` |  |  |
| `reset` | `boolean \| null` |  |  |
| `truncated` | `boolean \| null` |  |  |

**例**

```json
{
  "turn_id": "t-1",
  "step_id": "s-1",
  "status": "completed",
  "will_retry": false
}
```

## item

### `item/started`

Semantic item (message, tool call, custom record, or compaction) started. Oversized wire payloads are bounded at the outbound tap (`truncated: true` on truncated items/blocks; fetch full content via `session.get_item`).

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `required` | いいえ | いいえ | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `turn_id` | `string \| null` |  |  |
| `item_id` | `string` | はい |  |
| `step_id` | `string \| null` |  |  |
| `item` | `Item` | はい |  |

**例**

```json
{
  "turn_id": "t-1",
  "item_id": "it-1",
  "item": {
    "type": "assistant_message"
  }
}
```

### `item/delta`

Append-only incremental delta for an in-flight item.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `required` | はい | いいえ | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `turn_id` | `string` | はい |  |
| `item_id` | `string` | はい |  |
| `kind` | `ItemDeltaKind` | はい |  |
| `delta` | `string` | はい |  |
| `offset` | `integer ≥ 0` | はい |  |
| `tool_call_id` | `string \| null` |  |  |
| `partial` | `any` |  |  |
| `event` | `any` |  |  |
| `truncated` | `boolean \| null` |  |  |

**例**

```json
{
  "turn_id": "t-1",
  "item_id": "it-1",
  "kind": "text",
  "delta": "hi",
  "offset": 0
}
```

### `item/completed`

Authoritative completed item. For `tool_call`, `item.message` is the sole carrier of `content`/`details`. Oversized wire payloads project images to `{ type: "image", mime, path?, bytes, truncated: true }`, truncate long strings with `truncated: true`, or fall back to `item_omitted: true`; fetch full content via `session.get_item`.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `required` | いいえ | いいえ | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `turn_id` | `string \| null` |  |  |
| `item_id` | `string` | はい |  |
| `status` | `ItemStatus` | はい |  |
| `item` | `Item \| null` |  |  |
| `item_omitted` | `boolean \| null` |  |  |

**例**

```json
{
  "turn_id": "t-1",
  "item_id": "c-1",
  "status": "completed",
  "item": {
    "type": "tool_call",
    "tool_call_id": "c-1",
    "name": "read",
    "status": "completed",
    "message": {
      "role": "tool_result",
      "tool_call_id": "c-1",
      "tool_name": "read",
      "content": [],
      "is_error": false
    },
    "result_status": "success",
    "execution_status": "unknown"
  }
}
```

## queue

### `queue/changed`

Agent steer/follow-up queue snapshot changed.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `lossy` | いいえ | はい | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `items` | `QueueItem[]` | はい |  |
| `action` | `string \| null` |  |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**例**

```json
{
  "items": [
    {
      "input_id": "q-1",
      "lane": "steer",
      "text": "stop"
    }
  ]
}
```

## host

### `host/notice`

Informational or warning notice from the agent or host.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `lossy` | はい | いいえ | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `level` | `string` | はい |  |
| `text` | `string` | はい |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**例**

```json
{
  "level": "info",
  "text": "ok"
}
```

## subagent

### `subagent/started`

Subagent run started.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `required` | いいえ | いいえ | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `subagent_id` | `string` | はい |  |
| `agent_id` | `string \| null` |  |  |
| `name` | `string \| null` |  |  |
| `label` | `string \| null` |  |  |
| `run_id` | `string \| null` |  |  |
| `seq` | `integer \| null` |  |  |
| `task` | `string \| null` |  |  |
| `parent_session_id` | `string \| null` |  |  |
| `parent_session_epoch` | `string \| null` |  |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**例**

```json
{
  "subagent_id": "child-1",
  "parent_session_id": "parent-1"
}
```

### `subagent/progress`

Latest coalesced progress snapshot for a subagent.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `lossy` | はい | いいえ | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `subagent_id` | `string` | はい |  |
| `parent_session_id` | `string \| null` |  |  |
| `parent_session_epoch` | `string \| null` |  |  |
| `progress` | `object` | はい |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**例**

```json
{
  "subagent_id": "child-1",
  "parent_session_id": "parent-1",
  "progress": {
    "state": "working"
  }
}
```

### `subagent/completed`

Subagent run completed.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `required` | いいえ | いいえ | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `subagent_id` | `string` | はい |  |
| `agent_id` | `string \| null` |  |  |
| `name` | `string \| null` |  |  |
| `label` | `string \| null` |  |  |
| `status` | `string` | はい |  |
| `run_id` | `string \| null` |  |  |
| `seq` | `integer \| null` |  |  |
| `parent_session_id` | `string \| null` |  |  |
| `parent_session_epoch` | `string \| null` |  |  |
| `text` | `string \| null` |  |  |
| `error` | `EventError \| null` |  |  |
| `error_kind` | `string \| null` |  |  |
| `usage` | `any` |  |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**例**

```json
{
  "subagent_id": "child-1",
  "status": "completed",
  "parent_session_id": "parent-1"
}
```

## approval

### `approval/requested`

Approval prompt opened or requested for an action.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `required` | いいえ | いいえ | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `approval_id` | `string` | はい |  |
| `category` | `string` | はい |  |
| `tool_call_id` | `string \| null` |  |  |
| `snapshot` | `any` |  |  |
| `child_approval_id` | `integer \| null` |  |  |
| `truncated` | `boolean \| null` |  |  |

**例**

```json
{
  "approval_id": "ap-1",
  "category": "tool",
  "tool_call_id": "c-1",
  "snapshot": "snap-1"
}
```

### `approval/resolved`

Approval prompt resolved with its decision outcome.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `required` | いいえ | いいえ | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `approval_id` | `string` | はい |  |
| `category` | `string` | はい |  |
| `tool_call_id` | `string \| null` |  |  |
| `result` | `ApprovalResult` | はい |  |
| `truncated` | `boolean \| null` |  |  |

**例**

```json
{
  "approval_id": "ap-1",
  "category": "tool",
  "tool_call_id": "c-1",
  "result": {
    "call": {
      "tool_call_id": "c-1",
      "name": "write"
    },
    "decision": "allow",
    "source": "user"
  }
}
```

### `approval/changed`

Approval policy or revision changed.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `lossy` | いいえ | はい | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `policy` | `any` | はい |  |
| `revision` | `integer` | はい |  |

**例**

```json
{
  "policy": "ask",
  "revision": 1
}
```

### `approval/review-result`

Automated approval reviewer outcome.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `required` | いいえ | いいえ | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `category` | `string` | はい |  |
| `result` | `object` | はい |  |

**例**

```json
{
  "category": "exec",
  "result": {
    "call": {
      "name": "exec",
      "tool_call_id": "c-1"
    },
    "decision": "allow",
    "source": "reviewer"
  }
}
```

## status

### `status/changed`

Unified agent status changed.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `lossy` | いいえ | はい | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `state` | `string` | はい |  |
| `message` | `string` | はい |  |
| `session` | `object` | はい |  |
| `seq` | `integer ≥ 0` | はい |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**例**

```json
{
  "state": "idle",
  "message": "ready",
  "session": {},
  "seq": 1
}
```

## interaction

### `interaction/requested`

Interactive prompt opened and waiting for a responder.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `required` | いいえ | いいえ | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `interaction_id` | `string` | はい |  |
| `kind` | `InteractionKind` | はい |  |
| `title` | `string \| null` |  |  |
| `session_id` | `string \| null` |  |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**例**

```json
{
  "interaction_id": "int-1",
  "kind": "select",
  "title": "Pick"
}
```

### `interaction/resolved`

Interactive prompt finished with answered, cancelled, or unavailable.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `required` | いいえ | いいえ | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `interaction_id` | `string` | はい |  |
| `kind` | `InteractionKind` | はい |  |
| `outcome` | `InteractionOutcome` | はい |  |
| `title` | `string \| null` |  |  |
| `session_id` | `string \| null` |  |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**例**

```json
{
  "interaction_id": "int-1",
  "kind": "select",
  "outcome": "answered"
}
```

## compact

### `compact/completed`

Context compaction completed (succeeded, cancelled, failed, or stalled).

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `required` | いいえ | いいえ | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `cancelled` | `boolean \| null` |  |  |
| `changed` | `boolean` | はい |  |
| `compact_id` | `string` |  |  |
| `compactions` | `integer \| null` |  |  |
| `error` | `{ code: string, data?: any, message: string }` |  |  |
| `message` | `string \| null` |  |  |
| `offloaded_count` | `integer \| null` |  |  |
| `pruned_count` | `integer \| null` |  |  |
| `reason` | `string` | はい |  |
| `stalled` | `boolean \| null` |  |  |
| `status` | `"completed" \| "cancelled" \| "failed" \| "stalled"` | はい |  |
| `tokens_after` | `number` | はい |  |
| `tokens_before` | `number` | はい |  |

**例**

```json
{
  "changed": true,
  "compact_id": "c-1",
  "reason": "auto",
  "status": "completed",
  "tokens_after": 60,
  "tokens_before": 100
}
```

### `compact/stage`

Best-effort compaction pipeline stage diagnostic.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `lossy` | はい | いいえ | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `changed` | `boolean` |  |  |
| `phase` | `string` | はい |  |
| `reason` | `string` |  |  |
| `stage` | `string` | はい |  |
| `tokens` | `number` |  |  |
| `tokens_after` | `number` |  |  |
| `tokens_before` | `number` |  |  |

**例**

```json
{
  "phase": "before",
  "stage": "prune",
  "tokens": 100.0
}
```

### `compact/started`

Context compaction started.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `required` | いいえ | いいえ | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `compact_id` | `string` |  |  |
| `messages` | `integer` |  |  |
| `reason` | `string` | はい |  |

**例**

```json
{
  "compact_id": "c-1",
  "messages": 4,
  "reason": "manual"
}
```

## jobs

### `jobs/changed`

Background job state changed.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `lossy` | いいえ | はい | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `job` | `object` | はい |  |
| `truncated` | `boolean` |  |  |

**例**

```json
{
  "job": {
    "job_id": "j-1",
    "kind": "bash",
    "owner": "bash",
    "started_at_ms": 1700000000000,
    "status": "running"
  }
}
```

## model

### `model/changed`

Active model selection changed.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `lossy` | いいえ | はい | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `model_id` | `string` | はい |  |
| `provider_id` | `string` | はい |  |
| `ref` | `string` | はい |  |

**例**

```json
{
  "model_id": "gpt-5",
  "provider_id": "openai",
  "ref": "openai/gpt-5"
}
```

## permission

### `permission/changed`

Active permission preset or policy state changed.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `lossy` | いいえ | はい | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `label` | `string` |  |  |
| `partial` | `boolean` |  |  |
| `preset` | `string` |  |  |

**例**

```json
{
  "label": "DEFAULT",
  "preset": "default"
}
```

## session

### `session/changed`

Active session created, resumed, or forked.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `required` | いいえ | はい | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `path` | `string` |  |  |
| `reason` | `"resume" \| "new" \| "fork"` | はい |  |
| `session_id` | `string` | はい |  |

**例**

```json
{
  "reason": "new",
  "session_id": "s-1"
}
```

## ui

### `ui/card`

UI card emitted via ui.card (persistent cards are also written to session records).

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `required` | いいえ | いいえ | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `after_entry_id` | `string` |  |  |
| `block_id` | `integer` |  |  |
| `card_id` | `string` |  |  |
| `kind` | `string` | はい |  |
| `markdown` | `string` | はい |  |
| `meta` | `any` |  |  |
| `persistent` | `boolean` | はい |  |
| `title` | `string` | はい |  |

**例**

```json
{
  "kind": "info",
  "markdown": "**hi**",
  "persistent": false,
  "title": "Note"
}
```

### `ui/keymap-changed`

Runtime keymap bindings changed (set, delete, or detach cleanup).

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `lossy` | いいえ | はい | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `action` | `string \| boolean` |  |  |
| `context` | `string` |  |  |
| `key` | `string` |  |  |
| `session_id` | `string` |  |  |

**例**

```json
{
  "action": "transcript",
  "context": "editor",
  "key": "ctrl+t"
}
```

### `ui/print`

Ephemeral UI print output outside the durable session log.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `lossy` | はい | いいえ | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `lines` | `string[]` |  |  |
| `text` | `string` | はい |  |

**例**

```json
{
  "lines": [
    "hello"
  ],
  "text": "hello"
}
```

## usage

### `usage/changed`

Session token usage and cost totals changed.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `lossy` | いいえ | はい | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `cache_read` | `integer` | はい |  |
| `cache_write` | `integer` | はい |  |
| `cache_write_1h` | `integer` |  |  |
| `cost` | `number` | はい |  |
| `cost_unknown` | `boolean` |  |  |
| `input` | `integer` | はい |  |
| `last_cache_read` | `integer` | はい |  |
| `last_context` | `integer` | はい |  |
| `output` | `integer` | はい |  |
| `requests` | `integer` | はい |  |
| `side_requests` | `integer` | はい |  |
| `subscription_requests` | `integer` | はい |  |

**例**

```json
{
  "cache_read": 0.0,
  "cache_write": 0.0,
  "cost": 0.01,
  "input": 1.0,
  "last_cache_read": 0.0,
  "last_context": 10.0,
  "output": 2.0,
  "requests": 1.0,
  "side_requests": 0.0,
  "subscription_requests": 0.0
}
```

## web

### `web/providers-changed`

Registered web search or fetch providers changed.

| モード | 配信 | 無視可能 | 保持 | エラー時 |
|---|---|---|---|---|
| `emit` | `lossy` | いいえ | はい | `continue` |

| フィールド | 型 | 必須 | 説明 |
|---|---|---|---|
| `action` | `string` |  |  |
| `kind` | `string` | はい |  |
| `provider_id` | `string` |  |  |

**例**

```json
{
  "action": "added",
  "kind": "search",
  "provider_id": "brave"
}
```
