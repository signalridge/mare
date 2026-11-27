# Hooks, status line, and notifications

mare has no built-in shell-command hook runner in core; instead, hooks are Lua listeners (`mare.on`, `ctx:on`, `mare.policy.voter`, `ctx.policy:rewriter`) or external programs invoked by plugins such as `statusline`, `notify`, or community hook packages.

## Lifecycle hooks in `init.lua` and plugins

Subscribe to kernel events in `~/.config/mare/init.lua` with `mare.on` or vote at decision points with `mare.policy.voter`:

```lua
-- ~/.config/mare/init.lua
mare.on("turn/started", function(ev)
  mare.log("info", "turn started: " .. tostring(ev.turn_id))
end)

mare.policy.voter("tool/before", function(call)
  if call.name == "bash" and (call.arguments.command or ""):find("rm%s+%-rf%s+/%s*$") then
    return { decision = "deny", reason = "refusing rm -rf /" }
  end
end)
```

For full plugin-level control (`ctx:on`, `ctx.policy:rewriter`, `ctx.bus:waterfall`), see [Services and events](../extend/services-and-events.md).

### Key lifecycle and policy hook points

| Event / Point | Kind | Common use |
|---|---|---|
| `app/ready`, `app/shutdown` | Notify | Run setup after UI/plugins start or clean up before exit |
| `session/started`, `session/loaded`, `session/changed` | Notify | React when sessions open or switch |
| `input/submit`, `input/transform` | Notify / Waterfall | Inspect or rewrite user input before a turn starts |
| `request/prepare`, `request/ready`, `request/build` | Checked / Policy / Waterfall checked | Block or customize outbound model requests |
| `context/system`, `context/transform` | Waterfall | Append dynamic context or filter messages |
| `tool/rewrite`, `tool/before`, `tool/approval`, `tool/after` | Rewrite / Policy / Waterfall | Sanitize arguments, block dangerous calls, or transform tool outputs |
| `turn/started`, `turn/completed`, `agent/settled` | Notify | Track turn start/finish (`turn/completed` runs first; `agent/settled` fires after all its listeners return, carrying `busy`) |
| `option/changed`, `reload/completed` | Notify | React to runtime option edits and reloads |

### Error behavior

Error handling depends on the dispatch mode:

- **Ordinary notifications (`emit`)**: if a listener throws during a turn, mare logs the traceback and prints one short inline line below the turn (`plugin <name> hook <event> failed: ...`) without stopping other listeners or disabling the plugin.
- **Checked hooks (`emit_checked`, `waterfall_checked`)**: errors propagate to the caller. An error in `request/prepare` or `request/build` aborts the outbound model request before provider I/O.
- **Policy gates**: if a `request/ready` voter denies a prompt, mare does not send it and returns the text to the input box. A `tool/before` or `tool/approval` voter that errors or times out (`policy_timeout_ms`, default 30s) fails closed as `deny`.

### Shell-hook packages (`mare-plugins`)

If you want declarative JSON/shell hooks compatible with Claude Code and Codex hook event sets (`PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `SessionStart`, and similar), install a hook package from `signalridge/mare-plugins` via `/plugins` or `mare.plugins.add` rather than reimplementing process runners in `init.lua`.

## Custom status line (`statusline`)

The `statusline` plugin renders the bar below the input editor (`mode`, `model`, `effort`, `context`, `cwd`, `branch`, `pr`, `changes`, `cost`, `quota`).

### Built-in looks and toggles

Run `/statusline` inside a session to toggle individual segments (saved in `state.json`), or configure layouts in `init.lua`:

```lua
mare.plugin("statusline", {
  layout = "combined", -- "combined" | "classic" | "codex" | "kimi" | custom look table
  hidden = { "pr", "cost" },
  cwd_width = 24,
  gauge_width = 6,
  changed_files = true,
  pr = true,
  layout_opts = { sep = "space", flash = 0 },
})
```

### External status-line command

Set `command` to run your own script after startup, model changes, and each completed turn. It receives a JSON object on stdin (`session_id`, `transcript_path`, `cwd`, `width`, `model`, `context`, `cost`) and mare shows its first stdout line:

```lua
mare.plugin("statusline", {
  command = "~/bin/mare-statusline.sh",
  command_timeout_ms = 1000,
  command_replaces = false, -- set true to hide built-in segments
})
```

Plugins can also register status segments in Lua with `ctx.ui:status(id, { align = "right", order = 20, render = ... })`.

## Notifications (`notify`)

The `notify` plugin sends terminal notifications (OSC 9, OSC 777, Kitty OSC 99, tmux/screen passthrough, or bell, plus native desktop notifications on Windows outside Windows Terminal) when a turn exceeding `min_turn_ms` (default 5000 ms) finishes or when mare waits for user input.

```lua
mare.plugin("notify", {
  on_done = true,
  on_input = true,
  only_unfocused = true,
  min_turn_ms = 5000,
  protocol = "auto", -- "auto" | "bel" | "native"
  bell = false,
  dedupe_ms = 10000,
})
```

### External notification command (`notify.cmd`)

To trigger desktop notifications, webhooks, or sounds in any mode (TUI, headless, or RPC), set `cmd`. mare passes one JSON object on stdin (`type` as `"agent-turn-complete"` or `"input-required"`, `session-id`, `turn-id`, `cwd`, and `last-assistant-message` or `message`):

```lua
mare.plugin("notify", {
  cmd = { "notify-send-mare" },
  cmd_timeout_ms = 10000,
})
```

## Related

- [Services and events](../extend/services-and-events.md) for writing Lua event and policy hooks
- [Event reference](../reference/events.md) for generated public protocol event schemas
- [Appearance](ui.md) for themes, borders, and UI component overrides
