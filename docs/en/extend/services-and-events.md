# Services and events

How plugins talk to each other: providing and replacing services, the event dispatch styles, policy and rewrite events, built-in services and main events.

## Services

### Use a service

Name the services you need in `inject`. They are then available as `ctx.<name>`, or through `ctx:get(name)`. Call methods with a colon.

```lua
return {
  name = "show-model",
  inject = { "llm", "commands" },
  apply = function(ctx)
    ctx.commands:register("which-model", {
      description = "Show the current model",
      run = function()
        local m = ctx.llm:current()
        ui.notify(m and m.ref or "no model")
      end,
    })
  end,
}
```

`ctx.llm` reaches the current provider on every read. When that provider is reloaded or replaced, the plugins that inject it restart with it.

A service the plugin can run without stays out of `inject`. `ctx:get(name)` returns it, or `nil` while nobody provides it. To register something into it, use `ctx:inject({ name }, fn)`: `fn(ctx)` runs while the service is provided, what it registers through that `ctx` is withdrawn when the service goes away, and `fn` runs again for the next provider. The built-in `profile` plugin adds `/profile` this way.

```lua
return {
  name = "show-model",
  inject = { "llm" },
  apply = function(ctx)
    -- /which-model exists while the commands service does
    ctx:inject({ "commands" }, function(sub)
      sub.commands:register("which-model", {
        description = "Show the current model",
        run = function()
          local m = ctx.llm:current()
          ui.notify(m and m.ref or "no model")
        end,
      })
    end)
  end,
}
```

### Provide a service

Declare the name in `provide` and its methods in `contracts`, then provide it in `apply` with a `mare.Service` class: the constructor names the service, each method of the contract is a method of the class, and `ctx:plugin(Class)` provides it. Inside a method, `self.ctx` is the calling plugin's `ctx`, so what a method registers as an effect of `self.ctx` belongs to the caller and is withdrawn when the caller goes away. The built-in `commands` service registers commands this way.

```lua
-- ~/.config/mare/plugins/greeter.lua
return {
  name = "greeter",
  provide = { "greeter" },
  contracts = { greeter = { required = { "greet", "remember" } } },
  apply = function(ctx)
    local known = {}
    local Greeter = require("cordis").class("Greeter", mare.Service)
    function Greeter:constructor(c)
      Greeter.super.constructor(self, c, "greeter")
    end
    function Greeter:greet(who)
      return known[who] and ("hello again, " .. who) or ("hello " .. who)
    end
    -- remembered until the calling plugin goes away
    function Greeter:remember(who)
      return self.ctx:effect(function()
        known[who] = true
        return function() known[who] = nil end
      end)
    end
    ctx:plugin(Greeter)
  end,
}
```

```lua
-- ~/.config/mare/plugins/greeter-user.lua
return {
  name = "greeter-user",
  inject = { "greeter" },
  apply = function(ctx)
    ctx.greeter:remember("world")
    ui.notify(ctx.greeter:greet("world"))   -- hello again, world
  end,
}
```

A service is checked against its contract before it is published. With no contract, or a required method missing, it is refused: the service is not provided, and the reason is recorded as the plugin's problem. A contract for a name the built-ins already define cannot be redeclared.

A contract can also describe each method, which publishes it to callers outside Lua. Instead of the `required` list, give `methods`: per method a `doc`, `params` and `result` JSON Schemas, the positional `order`, and `expose = "both"`. The kernel checks arguments and results against the schemas and derives the JSON adapter from the contract, and the method becomes the RPC method `service.method`.

### Replace a built-in service

A service has one provider. A second plugin that declares the same name in `provide` is refused before it runs, the error names both files, and the first keeps the service. Nothing falls back: when the provider goes away, the service is gone until a plugin provides it again.

To replace a built-in service, turn its plugin off with `mare.plugin("status", { enabled = false })` in `init.lua` (this closes the name for every source, yours included), and provide the service from your own plugin, implementing every required method of its contract.

```lua
-- ~/.config/mare/plugins/my-status.lua
return {
  name = "my-status",
  provide = { "status" },
  apply = function(ctx)
    local Status = require("cordis").class("Status", mare.Service)
    function Status:constructor(c)
      Status.super.constructor(self, c, "status")
    end
    function Status:get()
      return { state = "idle", message = "", cwd = mare.cwd, pid = mare.pid, seq = 0, session = {} }
    end
    ctx:plugin(Status)
  end,
}
```

Or give your plugin the same name as the built-in plugin and set `override = true`; it then also inherits responsibility for the services of the plugin it replaces. If a replacement of an essential service fails to start, requests are blocked rather than run without it.

To change behaviour without replacing a service, prefer an event listener, or change one piece through its own API: `ctx.tools:shadow(name, def)` re-registers a modified copy of a tool, and the UI components can be wrapped (see [Appearance](../configure/ui.md)).

### Built-in services

| Service | Provided by | What it does |
|---|---|---|
| `agent` | `agent` | Runs the agent loop: `prompt` (with `follow_up` delivery), `steer`, `cancel`, `activity` (`agent:activity().busy`), `messages`. |
| `llm` | `llm` | Provider registry, model catalog, login, and parameters: `current`, `set_model`, `resolve`, `recent`, `cycle_param`, `login`, `logout`, `auth_keys`. |
| `session` | `session` | The session tree on disk: `info`, `create`, `load`, `fork`, `close`, `list`, `entries`, `record`, `records`. |
| `context` | `context` | System prompt assembly: `section`, `suppress`, `variable`, `system_prompt`, `skills`, `templates`. |
| `compact` | `compact` | Staged context compaction: `run`, `register_stage`, `measure`, `threshold`. |
| `tools` | `tools` | The tool registry and the one guarded execution path: `register`, `shadow`, `command`, `get`, `list`, `execute`, `disable`, `enable`. |
| `permissions` | `permissions` | Sandbox and approval presets: `preset`, `request`, `grants`. |
| `approvals` | `approvals` | Independent approval policy and reviewers: `policy`, `set`, `request`, `register_reviewer`. |
| `commands` | `commands` | Slash commands: `register` (with `alias_of`), `run`, `list`, `get`. |
| `ui` | `ui` | Plugin-scoped interface pieces: `widget`, `status`, `invalidate`, `key`, `action`, `renderer`, `style`. |
| `theme` | `theme-default` | Themes: `list`, `get`, `register`, `select`, `preview`, `cancel_preview`, `register_override`. |
| `jobs` | `jobs` | Background processes: `start`, `track`, `get`, `list`, `background`, `cancel` (exposed to models as the `job_kill` tool), `read`. |
| `usage` | `usage` | Token and cost totals: `totals`, `context_tokens`, `cost_text`, `rate_limits`. |
| `status` | `status` | The unified agent state: `get`. |
| `mcp` | `mcp` | MCP servers: `status`. |
| `changes` | `changes` | Per-turn file changes and rewind: `turns`, `diffs`, `rewind`. |
| `review` | `review` | Code review: `run`, `viewer`. |
| `images`, `copy`, `plugins`, `update-check`, `spill` | the plugin of the same name | Image display, clipboard copy, package catalog UI, update check, result storage. |

The Lua type files in the runtime directory (`mare runtime`) give the full signatures; the [Lua API](lua-api.md) page is generated from them.

### Session provider contract

The `session` service splits lifecycle orchestration from session storage so custom `session` providers work identically across TUI, CLI, and multi-session RPC/web/ACP hosts:

- **Host-provided lifecycle methods**: `session.create`, `session.load`, `session.fork`, and `session.close` are provided by the host orchestrator (`provider = "host"` in the `session` contract). The orchestrator owns project confinement, `request_id` idempotency receipts, `expected_epoch` checks, and runtime lifecycle for Lua, slash-command, and RPC callers alike. A custom `session` provider must not implement these four methods.
- **Startup arguments (`mare.args`) in multi-runtime hosts**: When a multi-runtime host (`mare --rpc`, web, ACP) creates, loads, or forks a session, the orchestrator spawns a dedicated runtime and populates `mare.args` before `app/ready`:
  - `session_id`: pre-assigned session ID to adopt when starting a new or forked session.
  - `resume`: path or selector of an existing session to load on startup.
  - `fork_session` (boolean, with optional `fork_before_entry`): fork from the resumed source instead of opening it in place.
  - `name`: initial session name.
  - `fork_images` (optional map, when `prepare_fork` returns in-memory images): pre-verified image payloads for a fork; the provider reads and clears `mare.args.fork_images` during startup.
  - If startup loading or forking fails, record the error in `session:info().resume_error` so the orchestrator can close and roll back the runtime cleanly.
- **Lua-only provider hooks (`expose = "lua"`)**:
  - `session.switch(opts)`: called by the orchestrator on **single-runtime** hosts to switch the active session in place (`opts.kind = "new" | "load" | "fork"`, with optional `cwd`, `name`, `session_id`, `path`, `before_item_id`, `id`) and return the new live session snapshot. Providers coordinate host identity, session-only trust revocation, and bound-call guards via `ctx.kernel:begin_session_switch` / `commit` (`handle:commit(session_id)` automatically marks an active session export call as committed). If a custom provider omits `switch`, single-runtime lifecycle calls fail with `unavailable`.
  - `session.prepare_fork()`: called by the orchestrator on a live source runtime before a multi-runtime fork to drain side requests, flush the journal, and return `{ path, images? }` (or `nil` if nothing is persisted). Omitting `prepare_fork` causes live multi-runtime forks to fail with `unavailable`.
- **`session.close` under single- vs multi-runtime capacity**:
  - **Multi-runtime hosts**: shuts down and unloads the target session's runtime, releases its lock, and returns `{}`.
  - **Single-runtime hosts**: closing the bound session (by exact ID, path, or unique prefix of at least 4 characters) switches the VM in place to a fresh empty session via `session.switch({ kind = "new" })` and releases the prior session's lock, returning `{}` without exiting the host or restarting plugins/MCP servers. Closing an already-unloaded session returns `{}`; ambiguous prefixes fail with `invalid_arguments`, unknown IDs with `unknown_session`, stale `expected_epoch` with `stale_session`, and active turns with `busy`.

## Events

### Listening

`ctx:on(event, fn, opts)` subscribes for the plugin's lifetime. Listeners run in registration order; `opts.prepend = true` puts one first. It returns a function that unsubscribes. `ctx:once(event, fn)` is removed after its first call. In `init.lua` use `mare.on(event, fn)`, the same kind of listener; to vote on a decision point such as `tool/before`, use `mare.policy.voter(point, fn, opts)`.

```lua
ctx:on("model/changed", function(ev)
  ui.notify("model is now " .. ev.ref)
end)
```

A waterfall listener receives `(payload, ev)`. Calling `ev.next(payload)` (or `ev(payload)`) invokes the rest of the chain with that value (`ev.next()` forwards the current value) and returns their result; whatever the listener returns becomes the result. A listener that omits `ev.next` ends the chain there.

```lua
ctx:on("context/system", function(payload, ev)
  payload.text = payload.text .. "\nAnswer in English."
  return ev.next(payload)
end)
```

### How an event is dispatched

The producer chooses the dispatch style. It decides what your return value means.

| Style | Producer calls | Listeners | Your return value |
|---|---|---|---|
| Notify | `ctx.bus:emit` | All, in order. | Ignored. An error is reported and the rest still run. |
| Checked notify | `ctx.bus:emit_checked` | All, in order. | An error stops the dispatch and reaches the producer. |
| Bail | `ctx.bus:bail` | In order, until one returns non-`nil`. | The first non-`nil` value is the result. |
| Waterfall | `ctx.bus:waterfall`, `ctx.bus:waterfall_checked` | The first registered is outermost; each calls `next` for the rest. | The value passed to `next` goes on; the outermost listener's return value is the result. `waterfall_checked` gives the producer the error of a listener that raised. |
| Policy | `ctx.policy:decide(point, args, opts)` | Voters, in order. | A vote. See below. With `opts.require_explicit_approval`, no voters or only `nil` votes give an `unavailable` deny. |
| Rewrite | `ctx.policy:rewrite(point, call)` | Rewriters, in order. | `nil` to keep the arguments, or a new arguments table. Returns the rewritten arguments directly, or raises `policy.rewrite_refused`. |

Plugins emit their own events with the same calls (`inject = { "bus" }`). Every emitted event must be declared in a contract (`contracts.<root>.events`); use `name/verb` names and keep them unique to your plugin. Each declared event carries an `expose` reach with three levels:

- `expose = "process"`: strictly in-process. Never enters the external event log and cannot be observed or hooked over RPC, regardless of `include`.
- `expose = "lua"` (default): visible to in-process Lua plugins, not emitted on the default public RPC/JSON stream. A primary RPC connection's `include` / hook subscription can opt in, so `"lua"` is not a confidentiality boundary.
- `expose = "both"`: emitted on both in-process Lua and the default public RPC/JSON event streams.

There is no separate local channel.

### Policy events

A decision point asks plugins to allow or refuse something. It fails closed.

- A voter returns `nil` (no objection), `"allow"`, `"deny"`, `"ask"`, or a table `{ decision = "deny", reason = "..." }`. Unknown fields or decisions make the vote invalid, and an invalid vote is a deny.
- The first `deny` ends the dispatch and is final. Otherwise `ask` beats `allow`.
- A voter that raises an error, or does not answer within its timeout, counts as a `deny`. The default timeout is 30 seconds. Set `timeout_ms` in the voter's options to change it; `0` removes the limit, which a voter that waits for a person needs.
- `slot = "name"` gives a stateful check one decision slot. Only the first registered voter of that slot runs.
- Votes never rewrite anything. Arguments are frozen before voting.

```lua
ctx.policy:voter("tool/before", function(call, info)
  local command = call.name == "bash" and call.arguments.command or ""
  if command:find("rm%s+%-rf%s+/%s*$") then
    return { decision = "deny", reason = "refusing rm -rf /" }
  end
end)
```

A plugin votes with the policy service's `voter`, as above (`inject = { "policy" }`); a listener of a decision point (`ctx:on`) is not a voter. In `init.lua` the same is `mare.policy.voter(point, fn, opts)`.

Rewriting happens earlier, in the `tool/rewrite` chain. Each rewriter gets a copy of the call with the arguments so far and returns `nil` or a new arguments table. An error, a timeout or a non-table result fails the rewrite, and the call is refused.

```lua
ctx.policy:rewriter("tool/rewrite", function(call)
  if call.name == "bash" then
    local args = {}
    for k, v in pairs(call.arguments) do args[k] = v end
    args.command = (args.command or ""):gsub("^%s+", "")
    return args
  end
end)
```

The decision points are `tool/before`, `tool/approval`, `net/approval`, `request/ready` and `permission/verdict`, plus `fs/edit-intent` and `fs/write-intent` before edit tools write.

### Main events

![mare single turn data flow](../../assets/diagrams/turn-dataflow.svg)

Public stream and protocol events are generated from the registry in [Event reference](../reference/events.md). Below are representative in-process lifecycle and hook events; payloads are Lua tables, and the comments in the runtime directory's `types/mare/events.lua` document the rest.

| Event | Style | Payload and meaning |
|---|---|---|
| `app/ready` | notify | The UI is ready. |
| `app/shutdown` | notify | Clean exit; listeners are awaited. |
| `session/started` | notify | `{ session_id, path, resumed }` |
| `session/closed` | notify | `{ session_id, cwd, reason, path? }` (`session_id`, `cwd`, and `reason` are required; `path` is optional), emitted only when a root host session retires (child hosts do not emit it), before dependent observers tear down. |
| `agent/step-ready` | policy | `{ id, step, cancel, turn_id }` before each model request; a deny stops the run. |
| `agent/step-committed` | notify | `{ step_id, step, turn_id }` after a step's participant state was committed. |
| `turn/completed` | notify | `{ turn_id, status, termination_reason, cancellation_cause?, text, structured, usage, error, ... }` when a turn finishes (`cancellation_cause` is `"user"`, `"parent"`, `"hook"`, or `"disposed"` when `termination_reason` is `"interrupted"`). |
| `agent/settled` | notify | `{ turn_id, busy }`, after every `turn/completed` listener of that run returned; `busy` is true while another run or an outer `turn/completed` dispatch is still in progress. |
| `agent/turn-stopping` | bail | Return a string or `{ message, source? }` to keep the loop going (`source` marks the continuation with `source` and `notification = true`). |
| `request/prepare` | checked notify | `{ model, cancel }`, before the first request of a prompt. An error stops the request. |
| `request/ready` | policy | A deny stops the request: shown as `not sent: <reason>`. |
| `input/submit` | notify | The user submitted input. |
| `input/transform` | waterfall | Change the input before it is sent. |
| `context/transform` | waterfall | Change the messages before the request. |
| `request/build` | waterfall checked | Change the outbound request; an error aborts the request before provider I/O. |
| `context/system` | waterfall | `{ text, model }`; mutate `payload.text` and return `ev.next(payload)`. |
| `tool/rewrite` | rewrite | Change a call's arguments before voting. |
| `tool/before` | policy | `(call, { side_effect, idempotent, unconfined })` |
| `tool/approval` | policy | Answers an `ask` with `{ decision = "allow" }`, `{ decision = "allow", grant = "allow_session" }` (or `"allow_amend"` with `amendment`), `{ decision = "deny" }` or `{ decision = "deny", abort = true }`. |
| `tool/after` | waterfall | Runs after the tool executes (`result.sandbox_denials` lists refusals), before `tool/result`. |
| `tool/result` | waterfall checked | Runs after `tool/after` and before default truncation; an error in a listener produces an error tool result. |
| `provider/request` | waterfall | `{ provider, url, headers, body, model }` before it is sent. |
| `llm/usage` | notify | Usage after every provider request (`usage.cache_write_1h?` subset of `cache_write`, `usage.cost?`). |
| `compact/before` | bail | `false` cancels a compaction. |
| `option/changed` | notify | `{ name, value }` after `mare.o.<name> = value` or a reload. |
| `option/apply` | checked | `{ name = "plugins.<p>.<path>", value }` for a runtime plugin option write, before it is announced: the owner applies it, and an error fails the write and puts everything back. |
| `theme/changed` | notify | A theme snapshot. |
| `theme/palette` | waterfall | Adjust a flavor's palette before styles are built. |
| `ui/caps` | waterfall | Add or override terminal capabilities. |
| `ui/commit-before` | waterfall | `{ lines }` about to enter the scrollback. |
| `plugins/completed` | notify | `{ action, name?, status }` after a package operation. |
| `reload/completed` | notify | `{ added, removed, replaced, reloaded, notices }` after `/reload`. |
| `kernel/reloaded` | notify | Plugins were reloaded. |
| `kernel/error` | notify | `{ message, plugin, event, what, error }`: a listener, task, timer or dispose hook failed. |

External programs can subscribe to events over RPC; see [RPC](../integrate/rpc.md). Declare contract events with `expose = "both"` to publish your own events to the public stream.
