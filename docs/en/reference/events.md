# Event reference

Built-in public protocol and contract events, with their payload schemas. See [JSON event stream](../integrate/json-stream.md) and [Services and events](../extend/services-and-events.md) for usage.

> This page is generated from the code by `scripts/gen_docs.py`. Do not edit it by hand.

Protocol version: **3.0**

## turn

### `turn/started`

Agent turn started.

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `required` | no | no | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `turn_id` | `string` | yes |  |
| `trigger` | `string` | yes |  |
| `input_item_ids` | `string[] \| null` |  |  |
| `text` | `string \| null` |  |  |
| `source` | `string \| null` |  |  |
| `truncated` | `boolean \| null` |  |  |

**Example**

```json
{
  "turn_id": "t-1",
  "trigger": "prompt"
}
```

### `turn/completed`

Agent turn completed after run-end durability confirmation.

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `required` | no | no | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `turn_id` | `string` | yes |  |
| `status` | `TurnStatus` | yes |  |
| `termination_reason` | `AgentTerminationReason \| null` |  |  |
| `cancellation_cause` | `CancellationCause \| null` |  | Origin of cancellation when `termination_reason` is `interrupted`. |
| `text` | `string \| null` |  |  |
| `structured` | `any` |  |  |
| `usage` | `object \| null` |  |  |
| `error` | `EventError \| null` |  |  |
| `error_kind` | `string \| null` |  |  |
| `not_sent` | `boolean \| null` |  |  |
| `truncated` | `boolean \| null` |  |  |

**Example**

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

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `required` | no | no | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `turn_id` | `string` | yes |  |
| `step_id` | `string` | yes |  |
| `turn` | `integer \| null` |  |  |
| `model` | `any` |  |  |
| `attempt` | `integer \| null` |  |  |
| `truncated` | `boolean \| null` |  |  |

**Example**

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

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `required` | no | no | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `turn_id` | `string` | yes |  |
| `step_id` | `string` | yes |  |
| `status` | `StepStatus` | yes |  |
| `stop_reason` | `string \| null` |  |  |
| `usage` | `any` |  |  |
| `error` | `EventError \| null` |  |  |
| `error_kind` | `string \| null` |  |  |
| `will_retry` | `boolean \| null` |  |  |
| `attempt` | `integer \| null` |  |  |
| `delay_ms` | `number \| null` |  |  |
| `reset` | `boolean \| null` |  |  |
| `truncated` | `boolean \| null` |  |  |

**Example**

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

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `required` | no | no | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `turn_id` | `string \| null` |  |  |
| `item_id` | `string` | yes |  |
| `step_id` | `string \| null` |  |  |
| `item` | `Item` | yes |  |

**Example**

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

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `required` | yes | no | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `turn_id` | `string` | yes |  |
| `item_id` | `string` | yes |  |
| `kind` | `ItemDeltaKind` | yes |  |
| `delta` | `string` | yes |  |
| `offset` | `integer ≥ 0` | yes |  |
| `tool_call_id` | `string \| null` |  |  |
| `partial` | `any` |  |  |
| `event` | `any` |  |  |
| `truncated` | `boolean \| null` |  |  |

**Example**

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

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `required` | no | no | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `turn_id` | `string \| null` |  |  |
| `item_id` | `string` | yes |  |
| `status` | `ItemStatus` | yes |  |
| `item` | `Item \| null` |  |  |
| `item_omitted` | `boolean \| null` |  |  |

**Example**

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

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `lossy` | no | yes | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `items` | `QueueItem[]` | yes |  |
| `action` | `string \| null` |  |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**Example**

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

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `lossy` | yes | no | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `level` | `string` | yes |  |
| `text` | `string` | yes |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**Example**

```json
{
  "level": "info",
  "text": "ok"
}
```

## subagent

### `subagent/started`

Subagent run started.

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `required` | no | no | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `subagent_id` | `string` | yes |  |
| `agent_id` | `string \| null` |  |  |
| `name` | `string \| null` |  |  |
| `label` | `string \| null` |  |  |
| `run_id` | `string \| null` |  |  |
| `seq` | `integer \| null` |  |  |
| `task` | `string \| null` |  |  |
| `parent_session_id` | `string \| null` |  |  |
| `parent_session_epoch` | `string \| null` |  |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**Example**

```json
{
  "subagent_id": "child-1",
  "parent_session_id": "parent-1"
}
```

### `subagent/progress`

Latest coalesced progress snapshot for a subagent.

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `lossy` | yes | no | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `subagent_id` | `string` | yes |  |
| `parent_session_id` | `string \| null` |  |  |
| `parent_session_epoch` | `string \| null` |  |  |
| `progress` | `object` | yes |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**Example**

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

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `required` | no | no | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `subagent_id` | `string` | yes |  |
| `agent_id` | `string \| null` |  |  |
| `name` | `string \| null` |  |  |
| `label` | `string \| null` |  |  |
| `status` | `string` | yes |  |
| `run_id` | `string \| null` |  |  |
| `seq` | `integer \| null` |  |  |
| `parent_session_id` | `string \| null` |  |  |
| `parent_session_epoch` | `string \| null` |  |  |
| `text` | `string \| null` |  |  |
| `error` | `EventError \| null` |  |  |
| `error_kind` | `string \| null` |  |  |
| `usage` | `any` |  |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**Example**

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

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `required` | no | no | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `approval_id` | `string` | yes |  |
| `category` | `string` | yes |  |
| `tool_call_id` | `string \| null` |  |  |
| `snapshot` | `any` |  |  |
| `child_approval_id` | `integer \| null` |  |  |
| `truncated` | `boolean \| null` |  |  |

**Example**

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

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `required` | no | no | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `approval_id` | `string` | yes |  |
| `category` | `string` | yes |  |
| `tool_call_id` | `string \| null` |  |  |
| `result` | `ApprovalResult` | yes |  |
| `truncated` | `boolean \| null` |  |  |

**Example**

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

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `lossy` | no | yes | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `policy` | `any` | yes |  |
| `revision` | `integer` | yes |  |

**Example**

```json
{
  "policy": "ask",
  "revision": 1
}
```

### `approval/review-result`

Automated approval reviewer outcome.

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `required` | no | no | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `category` | `string` | yes |  |
| `result` | `object` | yes |  |

**Example**

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

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `lossy` | no | yes | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `state` | `string` | yes |  |
| `message` | `string` | yes |  |
| `session` | `object` | yes |  |
| `seq` | `integer ≥ 0` | yes |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**Example**

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

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `required` | no | no | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `interaction_id` | `string` | yes |  |
| `kind` | `InteractionKind` | yes |  |
| `title` | `string \| null` |  |  |
| `session_id` | `string \| null` |  |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**Example**

```json
{
  "interaction_id": "int-1",
  "kind": "select",
  "title": "Pick"
}
```

### `interaction/resolved`

Interactive prompt finished with answered, cancelled, or unavailable.

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `required` | no | no | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `interaction_id` | `string` | yes |  |
| `kind` | `InteractionKind` | yes |  |
| `outcome` | `InteractionOutcome` | yes |  |
| `title` | `string \| null` |  |  |
| `session_id` | `string \| null` |  |  |
| `truncated` | `boolean \| null` |  | True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`. |

**Example**

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

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `required` | no | no | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `cancelled` | `boolean \| null` |  |  |
| `changed` | `boolean` | yes |  |
| `compact_id` | `string` |  |  |
| `compactions` | `integer \| null` |  |  |
| `error` | `{ code: string, data?: any, message: string }` |  |  |
| `message` | `string \| null` |  |  |
| `offloaded_count` | `integer \| null` |  |  |
| `pruned_count` | `integer \| null` |  |  |
| `reason` | `string` | yes |  |
| `stalled` | `boolean \| null` |  |  |
| `status` | `"completed" \| "cancelled" \| "failed" \| "stalled"` | yes |  |
| `tokens_after` | `number` | yes |  |
| `tokens_before` | `number` | yes |  |

**Example**

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

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `lossy` | yes | no | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `changed` | `boolean` |  |  |
| `phase` | `string` | yes |  |
| `reason` | `string` |  |  |
| `stage` | `string` | yes |  |
| `tokens` | `number` |  |  |
| `tokens_after` | `number` |  |  |
| `tokens_before` | `number` |  |  |

**Example**

```json
{
  "phase": "before",
  "stage": "prune",
  "tokens": 100.0
}
```

### `compact/started`

Context compaction started.

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `required` | no | no | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `compact_id` | `string` |  |  |
| `messages` | `integer` |  |  |
| `reason` | `string` | yes |  |

**Example**

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

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `lossy` | no | yes | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `job` | `object` | yes |  |
| `truncated` | `boolean` |  |  |

**Example**

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

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `lossy` | no | yes | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `model_id` | `string` | yes |  |
| `provider_id` | `string` | yes |  |
| `ref` | `string` | yes |  |

**Example**

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

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `lossy` | no | yes | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `label` | `string` |  |  |
| `partial` | `boolean` |  |  |
| `preset` | `string` |  |  |

**Example**

```json
{
  "label": "DEFAULT",
  "preset": "default"
}
```

## session

### `session/changed`

Active session created, resumed, or forked.

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `required` | no | yes | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `path` | `string` |  |  |
| `reason` | `"resume" \| "new" \| "fork"` | yes |  |
| `session_id` | `string` | yes |  |

**Example**

```json
{
  "reason": "new",
  "session_id": "s-1"
}
```

## ui

### `ui/card`

UI card emitted via ui.card (persistent cards are also written to session records).

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `required` | no | no | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `after_entry_id` | `string` |  |  |
| `block_id` | `integer` |  |  |
| `card_id` | `string` |  |  |
| `kind` | `string` | yes |  |
| `markdown` | `string` | yes |  |
| `meta` | `any` |  |  |
| `persistent` | `boolean` | yes |  |
| `title` | `string` | yes |  |

**Example**

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

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `lossy` | no | yes | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `action` | `string \| boolean` |  |  |
| `context` | `string` |  |  |
| `key` | `string` |  |  |
| `session_id` | `string` |  |  |

**Example**

```json
{
  "action": "transcript",
  "context": "editor",
  "key": "ctrl+t"
}
```

### `ui/print`

Ephemeral UI print output outside the durable session log.

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `lossy` | yes | no | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `lines` | `string[]` |  |  |
| `text` | `string` | yes |  |

**Example**

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

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `lossy` | no | yes | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `cache_read` | `integer` | yes |  |
| `cache_write` | `integer` | yes |  |
| `cache_write_1h` | `integer` |  |  |
| `cost` | `number` | yes |  |
| `cost_unknown` | `boolean` |  |  |
| `input` | `integer` | yes |  |
| `last_cache_read` | `integer` | yes |  |
| `last_context` | `integer` | yes |  |
| `output` | `integer` | yes |  |
| `requests` | `integer` | yes |  |
| `side_requests` | `integer` | yes |  |
| `subscription_requests` | `integer` | yes |  |

**Example**

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

| Mode | Delivery | Ignorable | Retain | On error |
|---|---|---|---|---|
| `emit` | `lossy` | no | yes | `continue` |

| Field | Type | Required | Description |
|---|---|---|---|
| `action` | `string` |  |  |
| `kind` | `string` | yes |  |
| `provider_id` | `string` |  |  |

**Example**

```json
{
  "action": "added",
  "kind": "search",
  "provider_id": "brave"
}
```
