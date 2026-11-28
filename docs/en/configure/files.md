# Configuration files

Configure mare with Lua: where init.lua lives, how configuration layers combine, splitting a config into modules, reloading, and what happens on errors.

## Where configuration lives

| Path | Purpose |
|---|---|
| `~/.config/mare/init.lua` | Your configuration. Created by `mare init`. |
| `~/.config/mare/lua/` | Modules you load with `require` from `init.lua`. |
| `~/.config/mare/plugins/` | Your own plugins. See [Plugins](../extend/plugins.md). |
| `<project>/.mare/init.lua` | Project configuration in the project resource root (nearest `.mare/` or `.mcp.json`, up to the git root). Runs only in a trusted project. |
| `<project>/.mare/lua/`, `<project>/.mare/plugins/` | Project modules and plugins. |
| `~/.local/state/mare/state.json` | Choices made at runtime (`/model`, `/effort`, `/theme`, `/config` edits). Never edit it by hand. |
| `~/.config/mare/mare-lock.json` | Package lock file. See [Packages](packages.md). |

`$XDG_CONFIG_HOME` and `$XDG_STATE_HOME` replace `~/.config` and `~/.local/state` when they are set. `MARE_HOME=<dir>` moves everything under `<dir>/config`, `<dir>/state`, `<dir>/data` and `<dir>/cache`, which is useful for a throwaway setup.

A project is trusted when you answered yes to the trust prompt, ran `mare trust` in it, or passed `--trust`. `mare trust --revoke` undoes the first two. `--no-trust` ignores the project for one run.

## Layer order

Every option can come from six layers. A later layer wins over an earlier one.

| # | Layer | Example |
|---|---|---|
| 1 | Defaults | The value listed in [Options](options.md). |
| 2 | `state.json` | Saved by `/model`, `/effort`, `/theme`. |
| 3 | User `init.lua` | `mare.setup { model = "..." }` |
| 4 | Project `.mare/init.lua` | Same API, trusted projects only. |
| 5 | Environment | `MARE_BORDER=single` |
| 6 | Command line | `--set border=single`, `-m`, `-e` |

Two details matter in practice:

- `state.json` only fills top-level options that `init.lua` did not set. If `init.lua` sets `models = {...}`, the saved `models` is ignored as a whole.
- Environment values replace the whole option. `--set` values are merged into it, so `--set plugins.tools.edit_mode=replace` changes one key and keeps the rest.

## A first init.lua

`mare init` writes a commented starting point and a `.luarc.json`. It never overwrites an existing `init.lua`.

```sh
mare init
```

```lua
-- ~/.config/mare/init.lua
mare.setup {
  model = "openai/gpt-6-luna",
  border = "single",
}

mare.plugin("tools", { bash_timeout_ms = 120000 })
```

`mare.setup` takes core options (`effort` is invocation-only via `--effort` or `MARE_EFFORT`, so a global `effort` in `init.lua` is ignored). Tables merge recursively with what is already set; lists and plain values replace it. The [Options](options.md) page lists every core option with its type and default.

## Plugin options

`mare.plugin(name, opts)` overrides the defaults of one plugin. It uses the same merge rule as `mare.setup`.

```lua
mare.plugin("statusline", { cwd_width = 32, changed_files = false })
mare.plugin("update-check", { enabled = false })
```

`enabled = false` turns a plugin off. An unknown plugin name or option key produces a warning with a "did you mean" hint. The same values can be given on the command line:

```sh
mare --set plugins.tools.edit_mode=replace
```

## Reading and writing options at runtime

`mare.o` reads an option (falling back to its default) and writes it. In `init.lua` a write is the same as `mare.setup`, except that a table is replaced, not merged. After startup, for example from `/lua`, a write takes effect immediately and emits the `option/changed` event.

```lua
mare.o.border = "double"
mare.o.plugins.url_format = "https://git.example.com/%s.git"   -- dotted write into a table option
local current = mare.o.model                          -- read; falls back to the default
```

## Modules and require

If `~/.config/mare/lua/` exists, `require` in `init.lua` loads from it. A name `a.b` resolves to `lua/a/b.lua` or `lua/a/b/init.lua`. Module names may contain letters, digits, `_` and `-`. A module sees the same `mare` API as `init.lua`.

```lua
-- ~/.config/mare/init.lua
require("keys")
require("looks")
```

```lua
-- ~/.config/mare/lua/keys.lua
mare.keymap.set("editor", "<M-g>", "transcript", { desc = "open the transcript" })
```

In a project, `.mare/lua/` is searched first, then the user directory, then the standard library. A module that requires itself, directly or through others, is an error. Each evaluation of the configuration starts with an empty module cache, so `/reload config` runs your modules again.

## Inspect the result

`/config` lists every option with its value and where it came from: `default`, `state.json`, a layer with file and line, `env MARE_<NAME>`, or `cli`. `/config border` filters by name. Plugin options you set appear in a second list, and configuration warnings at the end. In the full overlay, Enter edits a string, number or boolean option; the new value applies now and is saved to `state.json`, but a value set in `init.lua` still wins on the next start. `/settings` is an alias.

## Reload

| Command | Effect |
|---|---|
| `/reload config` | Runs `init.lua` and the project `init.lua` again. Reloads only plugins whose `plugins.<name>` options changed (and plugins that depend on them) and notifies changed core options. |
| `/reload` | Everything above, plus newly declared packages are installed, plugins of new packages start, plugins of removed packages stop, and every other plugin reloads. |
| `/reload <plugin>` | Reloads one plugin. |

`--set` values stay in force across a reload. A reload is refused while a model request is running or another reload is in progress; run it again afterwards. Package changes need the full `/reload`, and `watchdog_ms` is read only at startup.

A reload that fails to evaluate leaves the old configuration in place. These registrations are restored to what they were before the attempt: options and their sources, key bindings from `init.lua`, UI components registered by the config, entries added by plugin config functions such as `mare.tool.command`, and `mare.on` subscriptions. What is not undone is anything else your Lua did while it ran: files written, commands started, global variables set. Keep `init.lua` free of side effects.

## When init.lua fails

An error in `init.lua` does not stop mare from starting. The part of the file before the error ran; the rest did not.

- In the TUI a dialog names the file, the line and the first line of the error. You can open the file in your editor (`editor` option, else `$VISUAL` / `$EDITOR`), exit, or continue. If you continue, every model request is blocked until the file is fixed and `/reload` succeeds. The status line shows `config error · /reload`.
- With `-p`, `--json` or `--rpc` there is no dialog: mare prints the error and exits with code 2.

Mistakes that are not errors, such as an unknown option name or a value of the wrong type, are warnings. They appear in `/config`, and in headless and RPC modes on stderr as `mare: config warning: ...`. With `--strict-config`, those two modes exit with code 2 instead.

```sh
mare -p --strict-config "summarize README.md"
```

To start without any of your configuration, use `--clean`. It skips `init.lua`, project config, user and project plugins, packages and `state.json`, so only built-in plugins and command-line values remain.

## Editor completion

`mare init` also writes `~/.config/mare/.luarc.json`, which points LuaLS at the Lua type definitions for the `mare` API and at the type files shipped by installed packages. The definitions are copied to the runtime directory when the mare version changes; `mare runtime` prints it (`~/.local/share/mare/runtime`, definitions in `types/`). The built-in plugin sources are there too, read-only, for reading or copying into `~/.config/mare/plugins/`. If you edit `.luarc.json`, mare stops rewriting it.

## Sensitive values

Do not write API keys into `init.lua`. `mare login` stores credentials in `~/.config/mare/auth.json`. For a provider, `api_key` accepts an environment variable name or a command that prints the key:

```lua
mare.provider("openai", {
  base_url = "https://proxy.example.com/v1",
  api_key = { cmd = { "pass", "show", "proxy/openai" }, refresh_interval_ms = 3600000 },
})
```

`/config` and the RPC `options.get` method hide values whose key looks like a secret (`api_key`, `token`, `secret`, `password`, `credential`, `authorization`, `cookie`, `headers`, `query`, `env`) and URLs that carry a query string or user name. Mark your own plugin options the same way:

```lua
mare.config_sensitive("plugins.myplugin.private_value")
```

This only affects what mare displays. It does not encrypt the file.
