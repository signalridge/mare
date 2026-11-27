# RPC

`mare --rpc` is mare's full bidirectional programmatic interface over JSON-RPC 2.0 on stdin and stdout (alongside [headless mode](../use/headless.md), [ACP](acp.md), and the [web listener](../use/web.md)), letting any program that can start a process and read lines drive sessions, register tools and hooks, and answer approvals without a language-specific SDK. This page covers the transport, the handshake, the method groups, the callbacks mare sends back, and the peer socket.

## Start the server

```sh
mare --rpc --no-session
```

`--mode rpc` is the same thing. Every message is one JSON object on one line. stdout carries protocol messages only; logs go to the log file (see [Files](../reference/files.md)). The process serves one host over stdin and stdout: there is no port and no authentication, because only the parent process holds the pipe.

Closing stdin (or a read error) ends the session, cancels the active turn and exits with code 0. For one-shot prompts use headless mode instead (see [Headless mode](../use/headless.md)).

Print the JSON Schemas for the protocol with:

```sh
mare rpc schema > mare-rpc.schema.json
```

The output is a catalog, not a schema for one whole message. Its top-level keys are `envelope`, `methods`, `events`, `callbacks`, `notifications`, `client_notifications`, `errors`, `headless`, `ui_image_resource`, and `ui_output_image_key`, plus `protocol_version` (`events` merges native protocol events with events declared in built-in service contracts). Entries point at each other with `$ref`, so resolve them against the complete catalog. Field names and catalog keys use `snake_case` consistently across the protocol (`protocol_version`, `callback_timeout_ms`, `turn_id`, `session_id`, `registration_id`, events, and contract service methods).

## A minimal client

This Python client starts mare, runs the handshake, subscribes to events, sends one prompt and waits for the answer. It uses the standard library only. A runnable version with CLI flags is in [`examples/rpc`](../../../examples/rpc/README.md).

```python
import json
import subprocess

proc = subprocess.Popen(
    ["mare", "--rpc", "--no-session"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1,
)
next_id = 0


def send(msg):
    msg["jsonrpc"] = "2.0"
    proc.stdin.write(json.dumps(msg) + "\n")
    proc.stdin.flush()


def handle(msg):
    """Notifications and callbacks that arrive while we wait for a response."""
    method = msg["method"]
    if method == "event":
        print("event", msg["params"]["type"])
    elif method == "approval.request":
        send({"id": msg["id"], "result": {"decision": "deny"}})
    elif "id" in msg:  # a callback we did not declare support for
        send({"id": msg["id"], "error": {"code": -32601, "message": "not supported"}})


def call(method, params=None):
    global next_id
    next_id += 1
    rid = next_id
    send({"id": rid, "method": method, "params": params or {}})
    for line in proc.stdout:
        msg = json.loads(line)
        if "method" in msg:
            handle(msg)
        elif msg.get("id") == rid:
            if "error" in msg:
                raise RuntimeError(msg["error"])
            return msg["result"]
    raise RuntimeError("mare closed stdout")


call("initialize", {
    "protocol_version": "3.0",
    "client": {"name": "demo", "version": "0.1"},
    "capabilities": {"approvals": True},
})
session = call("session.create")
sid = session["session_id"]
call("events.subscribe", {"session_id": sid})
started = call("agent.prompt", {"session_id": sid, "input": "Summarize this repository"})
result = call("agent.wait", {"session_id": sid, "turn_id": started["turn_id"]})
print(result.get("text", ""))
call("shutdown")
proc.wait()
```

Requests from mare (callbacks) carry an `id` and a `method`; your client must answer each one. Notifications have a `method` and no `id`. The loop above handles both while it waits for its own response.

## Handshake

`initialize` must be the first request and runs at most once per connection. Any other request except `shutdown` fails with `-32600` until an `initialize` has succeeded on the connection. Use `connection.set_capabilities` to change capabilities later on the same connection.

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
  "protocol_version":"3.0",
  "client":{"name":"my-host","version":"1.0"},
  "capabilities":{"callbacks":true,"ui":true,"approvals":true,"elicitation":false},
  "callback_timeout_ms":30000,
  "approval_timeout_ms":120000}}
```

| Param | Meaning |
|---|---|
| `protocol_version` | Required `major.minor`. Missing, null, non-string, malformed, or different major fails with `invalid_arguments` / `unsupported_version` (and lets you retry `initialize`). |
| `client` | `{name, version?}`, informational. |
| `capabilities` | The callbacks you answer: `callbacks` (`callback.call` for tools, hooks, services, providers, web and renderers), `ui`, `approvals`, `elicitation`. All default to `false`. mare never sends a callback you did not declare. |
| `callback_timeout_ms` | Deadline for ordinary callbacks (ms). Default 30000, clamped to `1..=3600000`. |
| `approval_timeout_ms` | Deadline for human approvals. Without it an approval waits with no deadline. |

The result has `protocol_version`, `server` (`{name, version}`), `capabilities`, and (when the process opened a session at startup) `session` (whose `session_id` and `session_epoch` identify the startup session):

| Field | Meaning |
|---|---|
| `methods` | Host-to-mare methods this connection may call. |
| `callbacks` | The subset of your declared capabilities that is active. `ui`, `approvals` and `elicitation` are `false` on a peer connection. |
| `session` | Optional startup session snapshot opened by the process. Pass `session.session_id` as `session_id` on session-scoped calls. |

Minor versions add optional fields, methods and events. Removing or renaming something bumps the major version. There is no `2.x` compatibility layer in `3.0`.

## Host to mare methods

| Group | Methods |
|---|---|
| Sessions | `session.create`, `session.load` `{session_id?, path?}`, `session.get` `{session_id}`, `session.fork` `{session_id, before_item_id?}`, `session.list`, `session.list_items` `{session_id}`, `session.add_record`, `session.list_records`, `session.archive`, `session.unarchive`, `session.delete`, `session.rewind`, `session.search`, `session.snapshot`, `session.close` |
| Prompting | `agent.prompt` `{session_id, input}`, `agent.wait` `{session_id, turn_id?}`, `agent.steer` `{session_id, input}`, `agent.cancel` `{session_id}`, `agent.continue` `{session_id}`, `agent.get` `{session_id}`, `agent.list_messages` `{session_id}`, `agent.list_queue` / `agent.update_queued` / `agent.delete_queued`, `compact.run` `{session_id, instructions?}` |
| State | `state.snapshot`, `state.compare_exchange`, `status.get`, `llm.list_models`, `flags.list`, `flags.get`, `workspace.get` `{cwd}`, `workspace.set_trust` `{cwd, trusted, save?}`, `permissions.get` `{session_id}`, `permissions.list` `{session_id}`, `permissions.set` `{session_id, name, confirm?}`, `commands.run` `{line, input?, session_id?}` |
| Options | `options.get` `{session_id, keys?, describe?}`, `options.set` `{session_id, values}`, `config.describe` `{keys?}` |
| Extension | `events.list`, `events.subscribe` `{session_id?, scope?, session_epoch?, last_seq?, include?, exclude?}`, `baseline.read`, `tools.register` `{name, description, parameters, callbacks: ["execute"], side_effect?, scope?, session_id?, ...}`, `hooks.subscribe` `{event, session_id?, scope?, callbacks: ["handle"], ...}` |
| Services | `services.list`, `services.register`, `registrations.release` `{registration_id}`; every remote contract method directly as `service.method` (`theme.*`, `llm.*`, `permissions.*`, `usage.get`, `web.*`, `ui.*`; see `mare rpc schema`) |
| Output recovery | `spill.list_backends`, `spill.save`, `spill.read`, `spill.delete` |
| Lifecycle | `initialize`, `shutdown` |
| Peer | `peer.info`, `peer.message` (see [Peer sockets](#peer-sockets)) |

### Prompting

`agent.prompt` takes `{session_id, input, when_busy?, request_id?, output_schema?}` and returns `{status, turn_id?, input_id?, duplicate?}`. `input` is text or an array of content parts. When busy, the default is to queue; set `when_busy: "reject"` to reject instead. Call `agent.wait` with `{session_id, turn_id}` for the turn snapshot, or follow `event` notifications. Use `agent.steer` to supply input to a running turn and `agent.cancel` to cancel it. A `request_id` allows idempotent retries within a session.

#### Safe retries (`agent.prompt` and `agent.steer`)

An error reply does not always mean nothing happened. Group the error by `error.data.code`:

- **Rejected before commit:** nothing was queued, no turn started, no message was recorded, and the `request_id` was not remembered, so fix the cause (or reconnect) and send the request again: `parse_error`, `invalid_request`, `method_not_found`, `overloaded`, `denied`, `invalid_arguments`, `unknown_session`, `unsupported`, `request_id_conflict`, `busy`, `agent.rejected`, `agent.no_active_turn`, `agent.turn_mismatch`.
- **Indeterminate:** the input may already be committed: `internal`, `cancelled`, `stale_session`, `unavailable`, `invalid_result`, `persist_failed`, `timed_out`, `disconnected`, any other code, and a connection that closes before the reply arrives.

`request_id_conflict` leaves the original request and its result untouched. `busy` and `agent.rejected` come only from `agent.prompt`; `agent.no_active_turn` and `agent.turn_mismatch` come only from `agent.steer`.

After an indeterminate failure, send the same request again with the same `request_id`: a committed `agent.prompt` replies with its original `turn_id` or `input_id`, and a committed `agent.steer` with its original `input_id`, plus `duplicate: true`, instead of running twice. This holds when the frontend routes the input elsewhere (for example into a focused side conversation), and a resend that arrives while the first request is still running waits for its outcome. Without a `request_id`, check `agent.list_queue` and `agent.list_messages` before sending again.

### Options

`options.get` redacts sensitive values (credentials, provider keys and headers), the same as `/config`. With `describe: true` it returns, for each key, `{effective, default?, source?, description?, type?, deprecated?, sensitive, redacted}`. `options.set {session_id: "...", values: {model: "...", effort: "..."}}` is the RPC form of setting `mare.o.<name>`. On the primary connection's default session, committed `options.set` writes also update runtime defaults replayed only on `session.create` (as runtime writes, never elevated to the `cli` config layer); `session.load` and `session.fork` do not replay `options.set` defaults and instead restore the target session's recorded state. See [Options](../configure/options.md).

### Host tools

```json
{"jsonrpc":"2.0","id":2,"method":"tools.register","params":{
  "name":"lookup",
  "description":"Look up a ticket",
  "side_effect":"read",
  "parameters":{"type":"object","properties":{"ticket_id":{"type":"string"}},"required":["ticket_id"]},
  "callbacks":["execute"]}}
```

Each `tools.register` call registers one tool owned by the connection and returns `{registration_id}` (pass optional `scope: "all"|"session"` and `session_id` to target a single session). `name`, `description`, `parameters`, and `callbacks: ["execute"]` are required; classify side effects with `side_effect` (`"read"`, `"write"`, or `"external"`). Remote tools always run unconfined outside the local sandbox. You need `capabilities.callbacks: true`; mare invokes registered tools through `callback.call` with `callback: "execute"` and `arguments: {args, ctx: {tool_call_id?, idempotency_key?, cwd?}}`, expecting a string or `{content, is_error?, terminate?, details?}`. While in flight, stream partial progress with `callback.update {id, partial}` using the `callback.call` JSON-RPC request `id`. Release a registration with `registrations.release {registration_id}` (which returns `{}`); registrations a connection holds are also removed when it disconnects.

### Hooks

`hooks.subscribe {event, prepend?, timeout_ms?, scope?, session_id?, callbacks: ["handle"]}` requires `capabilities.callbacks: true` and returns `{registration_id}` (release via `registrations.release`). `prepend: true` puts the listener before earlier listeners. Do not pass a `mode` parameter to `hooks.subscribe`: the kernel infers the dispatch mode from `event` (`emit` for `agent/queue` and `status/changed`; `serial` for `request/prepare`; `waterfall` for `request/build`, `tool/result`, and `tool/rewrite`; `policy` for decision points such as `request/ready`, `tool/before`, `tool/approval`, and `net/approval`) and passes it in `callback.call` (`callback: "handle"`, `arguments: {event, mode, args}`).

### Services and output recovery

With `capabilities.callbacks: true`, a host may register a service through `services.register {name, methods, scope?, session_id?}` and receive `{registration_id, name}` (release via `registrations.release`). Service names use lowercase letters, digits, `_` and `-` and cannot collide with protocol roots. For a service that already has a contract (and whose methods can all cross the wire), omit `parameters`, `result`, `order`, `expose`, `access`, and `scope` (or repeat them verbatim). For a new service, declare `parameters`, `result`, optional `order`, `doc`, `optional`, `expose` (`"both"` default, or `"lua"`), `access` (`"primary"` default, `"peer"`, or `"peer_default"`), and `scope` (`"host"` default, `"workspace"`, or `"session"`). `services.list` returns visible remote contract methods; invoke them directly as `service.method`.

Recoverable tool output is stored per session through `spill.list_backends {}`, `spill.save {session_id, content|data_base64, backend?}` (returns `{spill_id, size}`; base64 bytes are capped at 8 MiB decoded), `spill.read {session_id, spill_id, encoding?, offset?, limit?}` (`encoding: "text"|"base64"`, returns `{content|data_base64, next_offset?, size?}`), and `spill.delete {session_id, spill_id}`. Spill IDs are bound to their owning session and never accept or expose host filesystem paths.

## Callbacks from mare

mare sends these as requests. Answer each with a result or an error.

| Callback | Params | Result |
|---|---|---|
| `callback.call` | `{registration_id, callback, session_id?, turn_id?, tool_call_id?, caller_id?, arguments}` | Declared return value for the registration: for tools `{content, is_error?, terminate?}` (when every result of a batch sets `terminate: true`, the run stops unless `agent/turn-stopping` continues); for hooks `null`, `{"next":true}`, or `{"value":...}`; for registered services/providers/web/renderers their declared JSON result |
| `approval.request` | `{session_id, turn_id?, kind: "tool"\|"network", approval_id?, tool_call_id?, tool?, arguments, command?, diff?, host?, cwd, reason?, side_effect?, unconfined, proposed_amendment?, requested_permissions?, scope?}` | `{decision, permissions?, scope?, amendment?, reason?}` |
| `mcp.elicitation` | `{session_id, turn_id?, server, request_id?, mode?, message, requested_schema?, url?, elicitation_id?, _meta?}` | `{action, content?}` |
| `ui.select` | `{title, body?, items: [{label, detail?}], selected?}` | `{index}` (`null` = cancelled) |
| `ui.input` | `{title, placeholder?, secret?}` | `{text}` (`null` = cancelled) |
| `ui.confirm` | `{prompt, default?}` | `{confirmed}` |
| `ui.editor` | `{title, prefill}` | `{text}` (`null` = cancelled) |
| `ui.questions` | `{questions}` | `{answers}` |

For a long-running `callback.call` (such as a host tool or registered provider stream), send client-to-mare `callback.update` notifications `{id, partial}`, where `id` is the pending `callback.call` request `id` and `partial` is a JSON value (for tools an object such as `{"output": "..."}`).

### Approvals

`approval.request` is sent only to the primary connection that declared `capabilities.approvals`. The `decision` values are:

| Decision | Effect |
|---|---|
| `allow` | Run it once. |
| `allow_session` | Run it; the permissions plugin remembers the grant for the session. |
| `allow_amend` | Run it and pass `amendment` to the permissions plugin to persist as a rule. |
| `deny` | Refuse. |
| `abort` | Refuse and end the current turn. |

No answer, an invalid answer, a timeout or a disconnect counts as `deny`. A decision value mare does not know is rejected and treated as `deny`. Network access needs an explicit affirmative answer; listeners that abstain do not grant it.

### Hook answers

For `waterfall`, reply `{"next":true}` to continue unchanged, `{"next":true,"value":...}` to rewrite and pass to the next listener, or `{"value":...}` to short-circuit; `{}` is `invalid_result`. For `tool/rewrite`, `value` is the call object with its `arguments` field, for example `{"value":{"id":"call-1","name":"tool","arguments":{"key":"new"}}}`. For `policy`, put the decision in `value`, for example `{"value":{"decision":"allow"}}` (also `ask` or `deny`, with optional vote fields). An absent value abstains; an unknown decision or field, other invalid answers, timeouts, disconnects, or withdrawal of `capabilities.callbacks` deny. The first deny stops dispatch.

### Cancellation and timeouts

mare sends `$/cancel_request {id}` when a callback is no longer active (and accepts `$/cancel_request {id}` from the client to cancel a cooperative in-flight request). Dismiss its UI and ignore any later answer. Ordinary callbacks time out after `callback_timeout_ms`. On a timeout or disconnect:

| Callback class | Result |
|---|---|
| Host tools | A tool error. |
| Policy hooks and `approval.request` | `deny`. |
| `serial` and `waterfall` hooks | The error propagates to the dispatch. |
| UI calls | Cancelled. |

## Notifications

| Notification | Params | Notes |
|---|---|---|
| `event` | `{type, data, session_id?, session_epoch?, seq, turn_id?}` | `turn_id` matches the `agent.prompt` result. `seq` increments contiguously per `(session_id, session_epoch)`. |
| `snapshot` | `{session_id?, session_epoch, seq, baseline_id, retained, parts}` | Sent immediately after `events.subscribe` when `mode = "baseline"`, and immediately after `lagged` for an evicted cursor. |
| `lagged` | `{from_seq, to_seq}` | Evicted cursor gap before the host cuts and sends a fresh `snapshot`. |
| `dropped` | `{count}` | Count of unretained `lossy` or refused events omitted from the stream. |
| `event` (`status/changed`) | `{type: "status/changed", data: {state, message, session, seq}}` | `state` is `idle`, `working` or `blocked`. Delivered as an `event` notification only when selected by `events.subscribe` (not sent as a standalone notification). Ignore a `seq` lower than the last one you saw. |
| `fs/changed` | `{watch_id, changed_paths, truncated?}` | Policy-filtered filesystem changes for active `fs.watch` subscriptions (`truncated: true` above 8192 entries). |
| `ui.notify` | `{text, ...}` | A message for the user. |
| `ui/select-updated`, `ui/select-closed`, `ui/component-updated` | see [RPC reference](../reference/rpc.md) | Incremental updates to an open `ui.select` modal or attached UI component. |
| `stream.failed` | `{error}` | The event stream is incomplete and the connection will close. |
| `$/cancel_request` | `{id}` | A callback or cooperative request was cancelled. |
| `peer.message_status` | see below | Peer sockets only. |

By default `event` carries the same public events as `mare --json` (see [JSON event stream](json-stream.md)), including contract events declared with `expose = "both"`. `events.subscribe {session_id: "...", include: [...]}` (or `{scope: "all", include: [...]}` on the primary connection) opts into more JSON kernel events by name or `prefix*`. Process-local and provider-private events are never delivered.

### Resuming and baselines

When `events.subscribe` answers `mode = "baseline"` (or after `lagged` + a new `snapshot` at `seq = S`), apply `snapshot.retained` and reconcile state in this order:

1. **Buffer live events** (`seq >= S + 1`) arriving while baseline pages are fetched.
2. **Page each part** in `snapshot.parts` (`"entries"`, `"active_items"`) via `baseline.read { baseline_id, part, cursor? }` until `next_cursor` is omitted. If any call fails with JSON-RPC error `error.data.code = "baseline.stale"` (idle >= 60 s, epoch change, superseded, or completed), re-subscribe to obtain a fresh baseline. (`baseline.unknown_part` and `baseline.item_too_large` report an invalid part name or an oversized item.)
3. **Apply buffered events** after reading the baseline, checking that any `item/delta.offset` (or block content offset) equals your local length; on mismatch, rebuild cleanly by calling `events.subscribe` without `last_seq`.

## Errors

Every error has `error.data.code`, a name from one vocabulary shared with Lua error values; `error.data.data` carries that code's additional data. The number only groups them: the five standard JSON-RPC numbers, `-32800` for cancellation and `-32000` for everything else. A service's own codes are `<service>.<name>`, declared by its contract and listed by `services.list`; an undeclared or unencodable error is `internal` with a bounded message. The full list is in the [RPC reference](../reference/rpc.md#error-codes).

| `data.code` | Number | Meaning |
|---|---|---|
| `parse_error` | `-32700` | Parse error. |
| `invalid_request` | `-32600` | Invalid request: also a call before `initialize`, a duplicate in-flight id, or a line over the size limit. |
| `method_not_found` | `-32601` | Unknown method, or a method not allowed on a peer socket. |
| `invalid_arguments` | `-32602` | Invalid params. |
| `internal` | `-32603` | Internal error; the message is bounded and carries no raw error value. |
| `cancelled` | `-32800` | Cancelled (`$/cancel_request`, owner release, shutdown). |
| `overloaded` | `-32000` | More than 256 requests in flight. |
| `busy` | `-32000` | A per-connection limit or the peer inbound queue is full. |
| `unsupported_version` | `-32000` | Unsupported protocol version. |
| `request_id_conflict` | `-32000` | A `agent.prompt` `request_id` was reused with different content. |
| `unsupported` | `-32000` | Unsupported capability or media input; `data.data.capability` names it. |
| `timed_out`, `disconnected`, `denied` | `-32000` | Service call timeout, callback disconnect, refusal for this connection. A remote implementation's error without a structured code is `internal`. |

## Limits

- A request line may be 64 MiB. A longer line is skipped and answered with `-32600`.
- Per connection, at most 4096 selected events and 64 MiB of serialized event records are admitted, and the output queue holds at most 4096 lines and 64 MiB. A record or line over its budget fails that connection.
- Selected events are fixed when admitted. A change to `events.subscribe` applies to later events.
- A response is queued after events that were already admitted.
- If an event cannot be encoded or delivered, mare sends `stream.failed` while output is still writable. If it is not, EOF or a truncated line is the signal.
- A failed or closed stdout cancels the active turn. A failed peer does not cancel the shared session.

## Peer sockets

Outbound peer clients are constructed in Lua through `mare.peers` (owned by the `peers` plugin contract; idle waiting uses `status/changed`). Each message identifies the session current at send time in `from.session_id`.

An interactive session also listens on `<state>/peers/<pid>.sock`, and `--rpc` or headless runs do too when started with `--peer`. Other programs of the same user can talk to it. The socket speaks a least-privilege subset of this protocol:

- The socket is mode 0600 in a 0700 directory (if the directory is a symlink or owned by another user, the session does not listen). On accept, mare verifies the connecting user id (Linux `SO_PEERCRED`, macOS `getpeereid`) and disconnects other users. On Windows the transport is a named pipe whose DACL admits only the current user's SID and SYSTEM (rejecting remote clients), with a random 128-bit pipe name written to the endpoint file at the socket path and verified by clients before use.
- A peer may call `initialize`, `peer.info`, `status.get`, `peer.message`, `events.subscribe` (explicitly `status/changed` only), and `shutdown` (which ends only that connection). Everything else fails with `-32601` (or `denied` for broader `events.subscribe` filters).
- To allow more, list exact method names (such as `"events.subscribe"` for the full event stream) in the `peer` option; wildcards and prefixes are not accepted:

```lua
mare.setup {
  peer = { name = "main", allow = { "agent.prompt", "tools.register", "events.subscribe" } },
}
```

`peer = false` turns the socket off. Primary-only methods (`options.set`, `commands.run`, `hooks.subscribe`, and other `access = "primary"` operations) are never grantable to peers. A peer never takes over the UI or approvals. Request lines on the peer socket are limited to 1 MiB, and at most 32 inbound messages wait for the user at once.

| Method | Params | Result |
|---|---|---|
| `peer.info` | `{}` | `{name, pid, cwd, version, allowed}` |
| `status.get` | `{}` | The unified status, the same shape as `status/changed`. |
| `peer.message` | `{from: {name?, pid?, cwd?, session_id?}, text, queue_only?, policy?}` | `{message_id, status}` |

What the sender says about itself in `from` and `policy` is shown to the receiving user and never grants anything. An empty `text` fails with `-32602`, and where peer credentials report the connecting process, a `from.pid` or registered `from.name` that disagrees with it is also refused with `-32602`. A message is held until the receiving user accepts or rejects it, and the answer arrives as a `peer.message_status {message_id, status}` notification with `delivered` or `rejected` (without a UI it is rejected). A delivered message reaches the agent as a follow-up inside a data boundary that marks it as coming from another session.

Running sessions register in `<state>/peers/<pid>.json` with `{name, pid, cwd, socket, mode, started, status, plugins?}`, where `plugins` stores optional opaque namespaces published via `mare.peers.publish(namespace, fields)` (rewritten at most every 200 ms; entries and sockets are removed on exit and dead entries are pruned when listed).

### Send a message from the shell

```sh
mare peer send --to main "tests are green on the integration branch"
printf 'see the diff in /tmp/x.patch' | mare peer send --to main
```

`--to` takes a socket path or a registered session name. Without it, mare uses `$MARE_PEER_SOCKET`, which tools and hooks started by a mare session inherit. The command prints the resulting status (`held`, `delivered` or `handled`) and exits 0 when the receiver accepted the message. Text comes from the arguments, or from stdin when there are none.

The plugin manager emits the in-process Lua event `plugins/completed { action, name?, status }`. See [Services and events](../extend/services-and-events.md) for its payload.
