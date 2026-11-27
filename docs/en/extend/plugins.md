# Plugins

What a mare plugin is: where plugins load from, the fields of a plugin definition, lifecycle, turning built-ins on and off, and how failures are contained.

## What a plugin is

A plugin is a Lua file that returns one table. The table describes the plugin; its `apply` function does the work.

```lua
-- ~/.config/mare/plugins/hello.lua
return {
  name = "hello",
  inject = { "commands" },
  config_schema = { greeting = { type = "string", default = "hello", desc = "what to say" } },
  apply = function(ctx, cfg)
    ctx.commands:register("hello", {
      description = "Say hello",
      run = function() ui.notify(cfg.greeting) end,
    })
  end,
}
```

The top level of the file may define local constants and helpers using whitelisted standard-library names (`string`, `table`, `math`, `pairs`, and similar) and return the plugin definition table. Reading non-whitelisted globals (such as `mare` or `require`) or writing globals is an error that names the offending line. Put runtime work in `apply`. [Writing a plugin](writing-a-plugin.md) builds a complete one step by step.

## Where plugins load from

mare collects plugin sources in this order. A later source has higher precedence.

| Order | Origin | Location |
|---|---|---|
| 1 | `builtin` | Compiled into mare in release builds; debug builds read `crates/mare-plugin/preset/plugins/` from disk. A read-only copy is in the runtime directory (`mare runtime`). |
| 2 | `package` | `plugins/` of each installed [package](../configure/packages.md), honouring its `only` list. |
| 3 | `user` | `~/.config/mare/plugins/` |
| 4 | `project` | `<project>/.mare/plugins/`, trusted projects only. |

In a directory, `name.lua` and `name/init.lua` are plugins. Entries whose names start with `_` or `.` are ignored.

Two plugins cannot share a name silently. A higher-precedence origin can replace a lower-precedence definition with the same name by setting `override = true`; without it, the later definition is refused and the error names both fixes (add `override = true`, or rename your plugin). If two definitions in the **same** origin share a name, both are refused and any lower-origin definition stays active — adding `override = true` does not resolve same-origin duplicates. Turning the earlier one off is no way around it: `enabled = false` closes the whole name, every source included. `/plugins` shows replaced plugins as `overrides built-in <name>`. A whole plugin definition can live in `<name>.lua` or `<name>/init.lua` (include `lua/` when it needs private modules) with `override = true`; placing a single helper module file alone does not shadow a plugin (for targeted test stubs, use `package.preload`).

`--clean` loads built-in plugins only.

## Modules and `require`

Every plugin — built-in and third-party alike — uses the same open `require` loader with owner-partitioned caches:

1. **Resolution order**: `package.loaded[name]` (global overrides and core modules) → `package.preload[name]` → the caller plugin's own `lua/` directory → core modules (`cordis*`, `mare_kernel*`, `mare_lib*`, `mare.test`) → other plugins by module root segment.
2. **Code is open, state goes through services**: cross-plugin `require` is for stateless helper code; runtime state and capabilities must go through Cordis services and events (`inject`/`provide`/`ctx:get`/`ctx.bus`). Call cross-plugin `require` inside `apply` (not at top-level definition load time) so all plugin `lua/` roots are indexed and dependency tracking records the edge for hot-reload. Modules run in their **provider's** environment, so any global written by a cross-plugin-required module stays in the provider's environment, not the caller's.
3. **Observable guarantees**:
   - Same-named modules in different plugins (e.g. `alpha/lua/util.lua` and `beta/lua/util.lua`) are cached in separate owner partitions and never collide.
   - If two plugins provide the same cross-plugin module root, cross-plugin `require` fails with an ambiguity error naming all providers (a plugin's own `lua/` still resolves locally first).
   - `mare_lib*` is owned by `core` (one shared instance per VM; reloading a plugin never evicts it).
   - Reloading a provider clears only its own module partition and automatically reloads plugins that `require`d its modules after the provider has rebuilt.
   - Overriding a built-in requires a full plugin replacement (`override = true`) or `package.preload`; public modules are declared with `---@meta <name>` in `types/`.

## The definition table

| Field | Type | Meaning |
|---|---|---|
| `name` | string | Plugin name. Defaults to the file name. |
| `inject` | string list | Services this plugin needs. It waits, without starting, until every one is provided. |
| `provide` | string list | Services it provides (a `mare.Service`, see [Services and events](services-and-events.md)). Two enabled plugins cannot declare the same one: the later in load order is refused, and the error names both files. |
| `contracts` | table | Method lists for new services it defines. See [Services and events](services-and-events.md). |
| `config_schema` | table | The plugin's options, one entry each: `{ type, default, desc, choices?, min?, max?, fields?, items?, values?, variants?, on_invalid?, apply? }`. It gives the defaults `apply` receives as `cfg` and the checks every entry point runs (init.lua, `--set`, `MARE_*`, runtime `mare.plugin` writes). An invalid value is reported and keeps the default. `type = "union", variants = {…}` takes a value matching one form exactly. `on_invalid = "fail"` keeps the plugin from starting instead of falling back to the default (for a restrictive setting such as an approval policy). A runtime write is first offered to the owner on `option/apply` (an error there, such as a failed save, fails the write and nothing changes), then announced on `option/changed`. Users set options with `mare.plugin(name, {...})`. |
| `config_api` | table | Functions that become `mare.<name>` in `init.lua`. See below. |
| `apply` | function | `apply(ctx, cfg)`. Runs at start and after each reload. May yield. |
| `required` | boolean | If it fails to start, every model request is refused until a full `/reload` succeeds. |
| `override` | boolean | May replace a same-name plugin from an earlier source. |
| `tags` | string list | Kind of plugin, read without running it. `"provider"` marks a model provider. |
| `wires` | string list | The wire formats it registers with the `llm` service. |
| `providers` | string list | The LLM provider names it registers with the `llm` service (read even when disabled, for status and attribution). |

`cfg` is a merged copy of the `config_schema` defaults and whatever the user set in `plugins.<name>`, with tables merged recursively. `mare.plugin("hello", { greeting = "hi" })` changes `cfg.greeting`; an unknown key or a wrong type produces a warning with a "did you mean" hint, so declare every option in `config_schema`.

### Config functions

`config_api` lets a plugin add functions to the configuration language. The key is a dotted path; the function receives the config table first.

```lua
config_api = {
  ["greeting.set"] = function(C, text) C.greeting = text end,
},
```

After the plugin is installed, `init.lua` can call `mare.greeting.set("hi")`, and `apply` reads `mare.config.greeting`. The built-in `tools` plugin uses this for `mare.tool.command`.

### Provider packs

Vendors are data, protocols are code, and names are only identifiers. A third-party plugin can ship OpenAI-compatible (or Responses-compatible) vendors as pure data entries without writing `stream` or `endpoint` callbacks, deriving `providers` from the same top-level table so declarations never drift:

```lua
local PRESETS = {
  acme = {
    label = "Acme AI",
    wire = "openai-chat",
    dialect = "deepseek",
    base_url = "https://api.acme.example/v1",
    api_key_env = "ACME_API_KEY",
    key_prefixes = { "acme-" },
    models_dev = false,
    default_model = "acme-v1",
    models = {
      { model_id = "acme-v1", context_window = 128000, max_output = 8192 },
    },
  },
}

local NAMES = {}
for name in pairs(PRESETS) do NAMES[#NAMES + 1] = name end
table.sort(NAMES)

return {
  name = "vendor-acme",
  inject = { "llm" },
  tags = { "provider" },
  providers = NAMES,
  apply = function(ctx)
    for _, name in ipairs(NAMES) do
      local entry = table.copy(PRESETS[name])
      entry.provider = name
      ctx.llm:register_provider(entry)
    end
  end,
}
```

Users can overlay any field of `acme` (such as `base_url`, `default_model`, `models`, or `wire_api`) with `mare.provider("acme", { ... })`, and unloading or disabling `vendor-acme` cleanly withdraws its providers and parameter rules.

## Lifecycle

![mare plugin lifecycle, override, and reload flow](../../assets/diagrams/plugin-lifecycle.svg)

1. mare reads every definition. A file that cannot be read is reported and skipped.
2. Plugins start in dependency order: a plugin waits until every service in its `inject` is provided, so the providers start first, then the rest in source order. A missing provider leaves its dependents waiting; plugins in a dependency cycle are rejected with `not loaded: dependency cycle`.
3. For each plugin, mare builds `cfg` and calls `apply(ctx, cfg)`.
4. Everything registered through `ctx` belongs to the plugin: listeners, services, timers, tasks, tools, commands, key bindings, status segments, and every `ctx:effect`.
5. On a reload or at exit, mare disposes the plugin, which withdraws all of that, and for a reload applies it again. The plugins that inject a service it provides restart with it.

See [Runtime guarantees](runtime-guarantees.md) for what mare promises about this lifecycle, each promise pinned by a test.

`ctx` has these methods and fields:

| Member | Does |
|---|---|
| `ctx.<service>` | A service named in `inject`, for example `ctx.tools`. |
| `ctx:get(name)` | A service by name, or `nil` while nobody provides it. For a service the plugin can run without. |
| `ctx:inject({ name, ... }, fn)` | Run `fn(ctx)` while the named services are provided. What it registers is withdrawn when one goes away, and `fn` runs again when they are back. |
| `ctx:on(event, fn, opts)`, `ctx:once(event, fn)` | Listen to an event. `opts.prepend = true` puts the listener first. Returns a function that removes it. |
| `ctx.bus:emit`, `:emit_checked`, `:bail`, `:waterfall`, `:waterfall_checked` | Dispatch events (`inject = { "bus" }`). See [Services and events](services-and-events.md). |
| `ctx.policy:voter`, `:rewriter`, `:decide` | Vote on, rewrite and ask decision points (`inject = { "policy" }`). |
| `ctx:plugin(Class)` | Provide a service: `Class` is a `mare.Service` subclass. |
| `ctx:effect(fn)` | Run `fn` now; the function it returns runs when the plugin goes away. |
| `ctx:spawn(fn)` | Run a background task, cancelled on dispose (`inject = { "task" }`). |
| `ctx:after(ms, fn)`, `ctx:every(ms, fn)` | Timers, cancelled on dispose (`inject = { "task" }`). |
| `ctx.logger:warn(...)`, `ctx.logger:error(...)` | Write to the log under the plugin's name. An error is also recorded as the plugin's problem and sent as `kernel/error`. |
| `ctx.kernel:reload(name)`, `:plugins()`, `:problems()` | Reload one plugin, or all; the plugin list and recorded failures (`inject = { "kernel" }`). |

A plugin that is applied is also described by `mare.plugins.status(name)` and `mare.plugins.list()`. `mare.plugins.failures()` returns what failed at startup or in the last full reload.

## Turn built-ins on and off

A single plugin, of any origin:

```lua
mare.plugin("update-check", { enabled = false })
```

A name is one slot across every source: `enabled = false` turns off the built-in and any package, user or project plugin of that name. Values from every layer are compared leaf by leaf, so a trusted project's `false` beats your `true`, and `--set plugins.<name>.enabled=…` beats both.

The minimal plugin set is a preset:

```lua
mare.setup { plugins = { preset = "minimal" } }
mare.plugin("jobs", { enabled = true }) -- add back what you need
```

`minimal` turns off every built-in tagged neither `minimal` nor `provider`. The tagged `minimal` plugins are `agent`, `agent-approvals`, `approval-review-model`, `approvals`, `baseline`, `commands`, `context`, `conversations`, `fs`, `keymap-default`, `llm`, `permissions`, `session`, `statusline`, `theme-default`, `tools`, `ui`, and `webserver`, plus every built-in `provider` plugin (`provider-anthropic`, `provider-google`, `provider-openai`, `provider-openai-compat`, `provider-opencode-go`, and in debug or `faux`-enabled builds, `provider-faux`). A plugin that replaced a built-in with `override = true`, and every package, user or project plugin, is not affected. An explicit `enabled` in any layer beats the preset. An unknown preset name is a configuration error: it is reported and no preset applies (`--strict-config` refuses to start). `preset` is reserved, so no plugin can be named so.

The built-in plugins, by area:

| Area | Plugins |
|---|---|
| Core | `agent`, `baseline`, `cells`, `compact`, `context`, `conversations`, `fs`, `jobs`, `llm`, `mcp`, `message`, `proc`, `query`, `query-disk`, `session`, `spill`, `subagent`, `terminal`, `tools`, `tools-apply-patch`, `web`, `web-fetch-http`, `webserver` |
| Providers | `provider-anthropic`, `provider-google`, `provider-openai`, `provider-openai-compat`, `provider-opencode-go`, `provider-faux` (debug builds or release builds compiled with the `faux` feature only) |
| Safety | `permissions`, `approvals`, `approval-review-model`, `agent-approvals`, `repeat-guard` |
| Interface | `ui`, `statusline`, `theme-default`, `keymap-default`, `commands`, `copy`, `images`, `file-ref`, `shell-mode`, `agents`, `review`, `changes`, `notify`, `usage`, `status` |
| Housekeeping | `plugins`, `profile`, `lua-console`, `update-check`, `onboarding`, `user-config` |

## Inspect and reload

`/plugins` lists every plugin with a filled dot when it is active, its origin, any override, and below that every recorded problem with its traceback. `mare doctor` includes plugin names defined twice.

| Command | Does |
|---|---|
| `/reload` | Full host reload: re-runs `init.lua`, installs new packages, starts new plugins, unloads removed ones, reloads the rest, and emits `reload/completed`. |
| `/reload config` | Re-runs `init.lua`, diffs plugin options, syncs enable/disable state, and retries pending items. |
| `/reload <name>` | Replaces the named plugin definition in place (keeps the old definition if the new one fails to load). |

A model request holds a lease from assembly to the end of streaming, so a reload cannot replace plugins under it. It is refused while a request is active; run it again afterwards. A plugin whose new definition fails to load keeps running on the old one.

In a debug build, built-in plugin sources are read from `crates/mare-plugin/preset/plugins/` on every start, so editing one and running `/reload` is enough. In a release build, `MARE_DEV_PLUGINS=<dir>` makes mare read each built-in from `<dir>/<name>.lua` when that file exists.

## When a plugin fails

Failures stay inside the plugin that caused them.

| What fails | What happens |
|---|---|
| Definition cannot load (syntax error, forbidden top-level access) | The plugin is skipped. mare warns `requests continue without it`, and the TUI status line shows `plugin <name> failed · /reload`. |
| `apply` raises an error | The plugin does not start. The error is recorded; other plugins start normally. |
| A service in `inject` is not provided | The plugin waits, and starts once a provider appears. `/reload` reports what it waits for. |
| A listener, task or timer raises an error | The error is reported as a notice and as a `kernel/error` event. The plugin keeps running. |
| A voter raises an error or runs out of time | It counts as a `deny`. |
| Lua runs without yielding for too long | The watchdog aborts the callback. |

A failure blocks every model request, with the reason shown, when the plugin provides one of the essential services (`agent`, `llm`, `tools`, `permissions`, `approvals`, `session`), declares `required = true`, or comes from a required package (`required = true`). The block lifts when a full `/reload` succeeds. Failures in optional plugins or optional packages report a warning and let requests continue.

The watchdog limit is `watchdog_ms`, 10000 by default. It is read before `init.lua` runs, so set it on the command line or in the environment: `mare --set watchdog_ms=20000` or `MARE_WATCHDOG_MS=20000`. A `pcall` around the runaway code does not swallow the abort.
