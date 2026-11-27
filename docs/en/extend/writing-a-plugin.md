# Writing a plugin

Build a small mare plugin step by step: a tool, a slash command, an event listener, a config option and a status-line segment, then test and publish it.

Read [Plugins](plugins.md) first for the definition fields.

## Do you need a plugin?

A tool that runs a command does not need one. Declare it in `init.lua`:

```lua
mare.tool.command {
  name = "grep",
  description = "Search file contents with ripgrep.",
  command = { "rg", "--line-number", "--no-heading", "--regexp={pattern}", "--", "{path}" },
  params = {
    pattern = { type = "string", required = true },
    path = { type = "string", default = "." },
  },
  readonly = true,
}
```

Each `{name}` in `command` is filled from the model's arguments. Without `readonly = true`, mare treats the command as having side effects. Use a plugin when you need Lua logic, events, commands or interface pieces.

## Step 1: the skeleton

Create `~/.config/mare/plugins/tally.lua`:

```lua
return {
  name = "tally",
  inject = { "tools", "commands", "ui" },
  apply = function(ctx, cfg)
    ctx.commands:register("tally", {
      description = "Show how many tool calls the current turn has made",
      run = function()
        ui.notify("tally is loaded")
      end,
    })
  end,
}
```

Start mare, or run `/reload` in a running session, then type `/tally`. A notice appears. `/plugins` lists `tally` with the origin `user`.

`inject` names the services the plugin uses. Each one is then a field of `ctx`: `ctx.commands`, `ctx.tools`, `ctx.ui`. `ui` without `ctx.` is the global namespace for building nodes and notices.

## Step 2: a tool

Register a tool with `ctx.tools:register(name, def)`. This one counts lines, words and characters in text the model passes in.

```lua
ctx.tools:register("text_stats", {
  snippet = "Count lines and words in a piece of text",
  description = "Count the lines, words and characters of the given text.",
  parameters = {
    type = "object",
    properties = { text = { type = "string", description = "The text to measure" } },
    required = { "text" },
  },
  side_effect = "read",
  summary = function(args)
    return ("TextStats(%d chars)"):format(#(args.text or ""))
  end,
  execute = function(args)
    local text = args.text or ""
    local _, lines = text:gsub("\n", "\n")
    local words = 0
    for _ in text:gmatch("%S+") do words = words + 1 end
    return { content = ("lines: %d\nwords: %d\nchars: %d"):format(lines + 1, words, #text) }
  end,
})
```

The tool goes through the same pipeline as the built-in ones: `tool/rewrite`, the argument check, the `tool/before` vote, approval, then `execute`. When the plugin is disposed the tool is withdrawn.

Fields of a tool definition:

| Field | Meaning |
|---|---|
| `description`, `parameters` | What the model sees. `parameters` is a JSON Schema object. Either may be a function of `(model, mode)`. |
| `execute(args, tctx)` | Runs the call. May yield. Returns a result table or a string. |
| `snippet`, `guidelines` | One line, and extra rules, for the system prompt. |
| `summary(args)` | The header shown in the transcript, such as `Read(src/main.rs)`. |
| `render(args, result, expanded)` | A custom node for the result. |
| `readonly` | No side effects. Consecutive read-only calls run in parallel. |
| `side_effect` | `"read"`, `"write"` (local files only) or `"external"`. Defaults to `"read"` when `readonly`, else `"external"`. |
| `idempotent` | Safe to run twice. A non-idempotent call that crashed mid-way resumes as outcome unknown, never replayed. |
| `unconfined` | Runs outside this process, so the local sandbox cannot confine it. |
| `enabled(model, edit_mode)` | Return `false` to hide the tool for a model. |
| `timeout_ms` | Time limit for the call. |

`tctx` carries `cancel` (a token to pass to blocking calls), `update(partial)` for progress, `cwd` and `tool_call_id`. A result table has `content` (a string, or a list of content blocks), and optionally `is_error`, `details`, `display` (a node), `no_truncate` and `max_line_chars`.

## Step 3: an event listener

Count finished tool calls on `item/completed`, and reset at the start of each turn:

```lua
local calls = 0

ctx:on("turn/started", function() calls = 0 end)
ctx:on("item/completed", function(data)
  if data.item and data.item.type == "tool_call" then
    calls = calls + 1
  end
end)
```

Listeners are removed when the plugin is disposed. Change the `/tally` command to report the number:

```lua
run = function()
  ui.notify(("%d tool call(s) in this turn"):format(calls))
end,
```

See [Services and events](services-and-events.md) for the event names and what a return value does.

## Step 4: a configuration option

Declare each option in `config_schema`. Its defaults, with what the user set, arrive as `cfg`; a value of the wrong type or out of range is reported and keeps the default.

```lua
config_schema = {
  warn_after = { type = "integer", default = 25, min = 1, desc = "notify when one turn makes more tool calls than this" },
  show_in_status = { type = "boolean", default = true, desc = "show the count in the status line" },
},
```

Use it in the listener:

```lua
ctx:on("item/completed", function(data)
  if data.item and data.item.type == "tool_call" then
    calls = calls + 1
    if calls == cfg.warn_after + 1 then
      ui.notify(("tally: more than %d tool calls in this turn"):format(cfg.warn_after), "warning")
    end
  end
end)
```

Users change it in `init.lua`. A key you did not declare produces a warning with a suggestion.

```lua
mare.plugin("tally", { warn_after = 10 })
```

`/reload config` reloads the plugin when its options change.

## Step 5: a status-line segment

`ctx.ui:status(id, def)` adds a segment. `render` returns a node, or `nil` to hide it. Call `ctx.ui:invalidate(id)` when the content changes. mare draws only when asked.

```lua
if cfg.show_in_status then
  ctx.ui:status("tally", {
    align = "right",
    order = 20,
    render = function()
      if calls == 0 then return nil end
      return ui.Line({ ui.Span(("calls: %d"):format(calls)):hl("MareDim") })
    end,
  })
end
```

Add `ctx.ui:invalidate("tally")` after each change to `calls`. `ctx.ui:widget(id, { slot = "above-editor", render = function(width, max_height) ... end })` draws a block above or below the input instead.

The snippets above all go inside `apply`, in order, with `local calls = 0` at the top. Keep `inject` and `config_schema` beside `apply` in the returned table. The result is one file of about 55 lines.

## Test it

- **Reload.** Edit the file, then run `/reload tally` for this plugin or `/reload` for everything. A plugin that fails to load keeps its previous version running.
- **Check the state.** `/plugins` shows whether it is active and prints the error with a traceback if it failed. Failures also appear in the status line as `plugin tally failed · /reload`.
- **Lua console.** `/lua` runs Lua in the running session, for example `/lua mare.plugins.status("tally")` or `/lua mare.o.border`. Without code it opens a prompt that accepts several lines. It exists in the TUI only and cannot be reached over RPC.
- **Log.** `ctx.logger:warn("...")` writes to the log under `~/.local/state/mare/log/`. `/debug` shows recent lines.
- **Without the TUI.** Load the plugin in a headless session and read stderr. A definition that does not load is reported as `mare: warning: plugin ...`:

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","client":{"name":"t"},"capabilities":{}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"shutdown","params":{}}' | mare --rpc >/dev/null
```

Use `MARE_HOME=$(mktemp -d)` in front to test against an empty configuration.

### Automated tests with `mare test`

`mare test` discovers every `*_spec.lua` file under the given paths (or the current directory, skipping `.git`, `target`, `node_modules`, and `snapshots`) and runs them in mare's built-in `mare.test` harness (`require("mare.test")`).

Use `t.host()` to spin up an isolated real plugin host (the same Cordis lifecycle tree, loader, event bus, and contract admission as production mare — with optional `builtins` or contract-checked fake services via `host:provide(name, impl, opts?)`).

Save `tests/tally_spec.lua` alongside `plugins/tally.lua`:

```lua
local t = require("mare.test")

t.describe("tally", function()
  t.it("counts lines, words, and chars via text_stats", function()
    local host = t.host({ builtins = { "tools", "commands", "ui" } })
    local plugin = dofile("plugins/tally.lua")
    host:load(plugin, { warn_after = 25, show_in_status = false })
    t.expect.settled(host)
    t.expect.eq(host:state("tally"), "ACTIVE")

    local tool = assert(host:get("tools"):get("text_stats"))
    local res = tool.execute({ text = "alpha beta\ngamma" })
    t.expect.eq(res.content, "lines: 2\nwords: 3\nchars: 16")
    t.expect.snapshot(res.content, "text_stats_sample")

    host:dispose()
    t.expect.eq(host:get("tools"), nil)
  end)
end)
```

Because `host` runs the real plugin lifecycle, the same fixture can test missing dependencies (`host:state(name) == "PENDING"` until `host:provide(...)` satisfies them), contract guards on fake services, cleanup on `host:dispose()`, and failed-reload rollback via `host:reload(name, def, cfg)`. Any test host not disposed explicitly is disposed automatically when its `t.it` case ends.

Because `tests/tally_spec.lua` includes a snapshot assertion, generate and review the baseline snapshot once with `-u`, then run ordinary checks from the plugin repository root:

```sh
mare test -u tests/tally_spec.lua # create or update snapshots/*.snap on the first run (inspect the file before committing)
mare test                         # discover and run all *_spec.lua files against saved snapshots
mare test tests/tally_spec.lua    # run a single spec file
mare test -f "text_stats"         # filter cases by substring
```

`expect.snapshot(value, name?)` writes `<spec_dir>/snapshots/<spec_stem>__<name>.snap`. Without `-u` (`--update-snapshots`), a mismatch fails the case and writes a `.snap.new` file for inspection. Exit codes are `0` (all passed), `1` (test failures), and `2` (usage, discovery, or syntax/load errors).

If you work on a built-in plugin in a checkout of the mare repository, `just run` starts a debug build that reads `crates/mare-plugin/preset/plugins/*.lua` from disk on every start. Edit the file and `/reload`; nothing is recompiled.

## Publish it as a package

A package is a git repository. Put the plugin in `plugins/`:

```
mare-tally/
  plugins/tally.lua
  mare-plugin.toml        # optional
```

```toml
# mare-plugin.toml
name = "mare-tally"
description = "Count tool calls per turn"
version = "0.1.0"
requires = ">=0.1"     # a mare older than this will not load the package
```

Push it and tag a release with a semver tag:

```sh
git tag v0.1.0 && git push --tags
```

Anyone installs it by declaring it in `init.lua` and running `mare plugin sync`. The options go under the plugin name:

```lua
mare.plugins.add {
  { "you/mare-tally", opts = { tally = { warn_after = 10 } } },
}
```

A repository can hold several plugins, and a user can load some of them with `only = { "tally" }`. A package may also ship `skills/`, `prompts/`, `themes/` and `types/` (Lua type definitions). See [Packages](../configure/packages.md).
