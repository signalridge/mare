# 事件参考

内置公开协议事件和契约事件及其载荷 schema。用法见 [JSON 事件流](../integrate/json-stream.md) 与 [服务与事件](../extend/services-and-events.md)。

> 本页由 `scripts/gen_docs.py` 从代码生成，请勿手工修改。说明文字来自代码，保持英文。

协议版本: **3.0**

## turn

### `turn/started`

Agent turn started.

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `required` | 否 | 否 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `turn_id` | `string` | 是 |  |
| `trigger` | `string` | 是 |  |
| `input_item_ids` | `string[] \| null` |  |  |
| `text` | `string \| null` |  |  |
| `source` | `string \| null` |  |  |
| `truncated` | `boolean \| null` |  |  |

**示例**

```json
{
  "turn_id": "t-1",
  "trigger": "prompt"
}
```

### `turn/completed`

Agent turn completed after run-end durability confirmation.

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `required` | 否 | 否 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `turn_id` | `string` | 是 |  |
| `status` | `TurnStatus` | 是 |  |
| `termination_reason` | `AgentTerminationReason \| null` |  |  |
| `cancellation_cause` | `CancellationCause \| null` |  | Origin of cancellation when `termination_reason` is `interrupted`. |
| `text` | `string \| null` |  |  |
| `structured` | `any` |  |  |
| `usage` | `object \| null` |  |  |
| `error` | `EventError \| null` |  |  |
| `error_kind` | `string \| null` |  |  |
| `not_sent` | `boolean \| null` |  |  |
| `truncated` | `boolean \| null` |  |  |

**示例**

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

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `required` | 否 | 否 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `turn_id` | `string` | 是 |  |
| `step_id` | `string` | 是 |  |
| `turn` | `integer \| null` |  |  |
| `model` | `any` |  |  |
| `attempt` | `integer \| null` |  |  |
| `truncated` | `boolean \| null` |  |  |

**示例**

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

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `required` | 否 | 否 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `turn_id` | `string` | 是 |  |
| `step_id` | `string` | 是 |  |
| `status` | `StepStatus` | 是 |  |
| `stop_reason` | `string \| null` |  |  |
| `usage` | `any` |  |  |
| `error` | `EventError \| null` |  |  |
| `error_kind` | `string \| null` |  |  |
| `will_retry` | `boolean \| null` |  |  |
| `attempt` | `integer \| null` |  |  |
| `max_attempts` | `integer \| null` |  |  |
| `delay_ms` | `number \| null` |  |  |
| `reset` | `boolean \| null` |  |  |
| `truncated` | `boolean \| null` |  |  |

**示例**

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

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `required` | 否 | 否 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `turn_id` | `string \| null` |  |  |
| `item_id` | `string` | 是 |  |
| `step_id` | `string \| null` |  |  |
| `item` | `Item` | 是 |  |

**示例**

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

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `required` | 是 | 否 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `turn_id` | `string` | 是 |  |
| `item_id` | `string` | 是 |  |
| `kind` | `ItemDeltaKind` | 是 |  |
| `delta` | `string` | 是 |  |
| `offset` | `integer ≥ 0` | 是 |  |
| `tool_call_id` | `string \| null` |  |  |
| `partial` | `any` |  |  |
| `event` | `any` |  |  |
| `truncated` | `boolean \| null` |  |  |

**示例**

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

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `required` | 否 | 否 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `turn_id` | `string \| null` |  |  |
| `item_id` | `string` | 是 |  |
| `status` | `ItemStatus` | 是 |  |
| `item` | `Item \| null` |  |  |
| `item_omitted` | `boolean \| null` |  |  |

**示例**

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

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `lossy` | 否 | 是 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `items` | `QueueItem[]` | 是 |  |
| `action` | `string \| null` |  |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**示例**

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

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `lossy` | 是 | 否 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `level` | `string` | 是 |  |
| `text` | `string` | 是 |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**示例**

```json
{
  "level": "info",
  "text": "ok"
}
```

## subagent

### `subagent/started`

Subagent run started.

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `required` | 否 | 否 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `subagent_id` | `string` | 是 |  |
| `agent_id` | `string \| null` |  |  |
| `name` | `string \| null` |  |  |
| `label` | `string \| null` |  |  |
| `run_id` | `string \| null` |  |  |
| `seq` | `integer \| null` |  |  |
| `task` | `string \| null` |  |  |
| `parent_session_id` | `string \| null` |  |  |
| `parent_session_epoch` | `string \| null` |  |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**示例**

```json
{
  "subagent_id": "child-1",
  "parent_session_id": "parent-1"
}
```

### `subagent/progress`

Latest coalesced progress snapshot for a subagent.

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `lossy` | 是 | 否 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `subagent_id` | `string` | 是 |  |
| `parent_session_id` | `string \| null` |  |  |
| `parent_session_epoch` | `string \| null` |  |  |
| `progress` | `object` | 是 |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**示例**

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

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `required` | 否 | 否 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `subagent_id` | `string` | 是 |  |
| `agent_id` | `string \| null` |  |  |
| `name` | `string \| null` |  |  |
| `label` | `string \| null` |  |  |
| `status` | `string` | 是 |  |
| `run_id` | `string \| null` |  |  |
| `seq` | `integer \| null` |  |  |
| `parent_session_id` | `string \| null` |  |  |
| `parent_session_epoch` | `string \| null` |  |  |
| `text` | `string \| null` |  |  |
| `error` | `EventError \| null` |  |  |
| `error_kind` | `string \| null` |  |  |
| `usage` | `any` |  |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**示例**

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

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `required` | 否 | 否 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `approval_id` | `string` | 是 |  |
| `category` | `string` | 是 |  |
| `tool_call_id` | `string \| null` |  |  |
| `snapshot` | `any` |  |  |
| `child_approval_id` | `integer \| null` |  |  |
| `truncated` | `boolean \| null` |  |  |

**示例**

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

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `required` | 否 | 否 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `approval_id` | `string` | 是 |  |
| `category` | `string` | 是 |  |
| `tool_call_id` | `string \| null` |  |  |
| `result` | `ApprovalResult` | 是 |  |
| `truncated` | `boolean \| null` |  |  |

**示例**

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

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `lossy` | 否 | 是 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `policy` | `any` | 是 |  |
| `revision` | `integer` | 是 |  |

**示例**

```json
{
  "policy": "ask",
  "revision": 1
}
```

### `approval/review-result`

Automated approval reviewer outcome.

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `required` | 否 | 否 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `category` | `string` | 是 |  |
| `result` | `object` | 是 |  |

**示例**

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

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `lossy` | 否 | 是 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `state` | `string` | 是 |  |
| `message` | `string` | 是 |  |
| `session` | `object` | 是 |  |
| `seq` | `integer ≥ 0` | 是 |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**示例**

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

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `required` | 否 | 否 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `interaction_id` | `string` | 是 |  |
| `kind` | `InteractionKind` | 是 |  |
| `title` | `string \| null` |  |  |
| `session_id` | `string \| null` |  |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**示例**

```json
{
  "interaction_id": "int-1",
  "kind": "select",
  "title": "Pick"
}
```

### `interaction/resolved`

Interactive prompt finished with answered, cancelled, or unavailable.

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `required` | 否 | 否 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `interaction_id` | `string` | 是 |  |
| `kind` | `InteractionKind` | 是 |  |
| `outcome` | `InteractionOutcome` | 是 |  |
| `title` | `string \| null` |  |  |
| `session_id` | `string \| null` |  |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**示例**

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

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `required` | 否 | 否 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cancelled` | `boolean \| null` |  |  |
| `changed` | `boolean` | 是 |  |
| `compact_id` | `string` |  |  |
| `compactions` | `integer \| null` |  |  |
| `error` | `{ code: string, data?: any, message: string }` |  |  |
| `message` | `string \| null` |  |  |
| `offloaded_count` | `integer \| null` |  |  |
| `pruned_count` | `integer \| null` |  |  |
| `reason` | `string` | 是 |  |
| `stalled` | `boolean \| null` |  |  |
| `status` | `"completed" \| "cancelled" \| "failed" \| "stalled"` | 是 |  |
| `tokens_after` | `number` | 是 |  |
| `tokens_before` | `number` | 是 |  |

**示例**

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

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `lossy` | 是 | 否 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `changed` | `boolean` |  |  |
| `phase` | `string` | 是 |  |
| `reason` | `string` |  |  |
| `stage` | `string` | 是 |  |
| `tokens` | `number` |  |  |
| `tokens_after` | `number` |  |  |
| `tokens_before` | `number` |  |  |

**示例**

```json
{
  "phase": "before",
  "stage": "prune",
  "tokens": 100.0
}
```

### `compact/started`

Context compaction started.

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `required` | 否 | 否 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `compact_id` | `string` |  |  |
| `messages` | `integer` |  |  |
| `reason` | `string` | 是 |  |

**示例**

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

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `lossy` | 否 | 是 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `job` | `object` | 是 |  |
| `truncated` | `boolean` |  |  |

**示例**

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

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `lossy` | 否 | 是 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `model_id` | `string` | 是 |  |
| `provider_id` | `string` | 是 |  |
| `ref` | `string` | 是 |  |

**示例**

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

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `lossy` | 否 | 是 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `label` | `string` |  |  |
| `partial` | `boolean` |  |  |
| `preset` | `string` |  |  |

**示例**

```json
{
  "label": "DEFAULT",
  "preset": "default"
}
```

## session

### `session/changed`

Active session created, resumed, or forked.

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `required` | 否 | 是 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `path` | `string` |  |  |
| `reason` | `"resume" \| "new" \| "fork"` | 是 |  |
| `session_id` | `string` | 是 |  |

**示例**

```json
{
  "reason": "new",
  "session_id": "s-1"
}
```

## ui

### `ui/card`

UI card emitted via ui.card (persistent cards are also written to session records).

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `required` | 否 | 否 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `after_entry_id` | `string` |  |  |
| `block_id` | `integer` |  |  |
| `card_id` | `string` |  |  |
| `kind` | `string` | 是 |  |
| `markdown` | `string` | 是 |  |
| `meta` | `any` |  |  |
| `persistent` | `boolean` | 是 |  |
| `title` | `string` | 是 |  |

**示例**

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

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `lossy` | 否 | 是 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `action` | `string \| boolean` |  |  |
| `context` | `string` |  |  |
| `key` | `string` |  |  |
| `session_id` | `string` |  |  |

**示例**

```json
{
  "action": "transcript",
  "context": "editor",
  "key": "ctrl+t"
}
```

### `ui/print`

Ephemeral UI print output outside the durable session log.

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `lossy` | 是 | 否 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `lines` | `string[]` |  |  |
| `text` | `string` | 是 |  |

**示例**

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

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `lossy` | 否 | 是 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cache_read` | `integer` | 是 |  |
| `cache_write` | `integer` | 是 |  |
| `cache_write_1h` | `integer` |  |  |
| `cost` | `number` | 是 |  |
| `cost_unknown` | `boolean` |  |  |
| `input` | `integer` | 是 |  |
| `last_cache_read` | `integer` | 是 |  |
| `last_context` | `integer` | 是 |  |
| `output` | `integer` | 是 |  |
| `reasoning` | `integer` |  |  |
| `requests` | `integer` | 是 |  |
| `side_requests` | `integer` | 是 |  |
| `subscription_requests` | `integer` | 是 |  |

**示例**

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

| 模式 | 投递 | 可忽略 | 保留快照 | 出错时 |
|---|---|---|---|---|
| `emit` | `lossy` | 否 | 是 | `continue` |

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `action` | `string` |  |  |
| `kind` | `string` | 是 |  |
| `provider_id` | `string` |  |  |

**示例**

```json
{
  "action": "added",
  "kind": "search",
  "provider_id": "brave"
}
```
