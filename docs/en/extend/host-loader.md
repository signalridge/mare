# The `loader` host plugin

The plugin tree runs on cordis-lua's loader (`cordis.loader`). mare wraps it in a host plugin,
`mare_kernel.plugins.loader`, which the boot fragment (`boot.lua`) uses on the root context. It
provides the service `loader`. Types: `types/mare/host/loader.lua`.

Host plugins use only typed, documented primitives. The tables below are passed to the
host plugin as its config by the boot fragment; the live `prims` tables are not globals and
are not exposed as a `require`-able module (while `mare_kernel.*` source modules themselves
follow the open `require` model and are internal/undeclared in `types/`).

## Config

```lua
root:use(require("mare_kernel.plugins.loader"), {
  component = component, -- fun(name): the cordis component for a plugin name
  prims = prims,         -- row primitives; prims.refused and prims.profile_record are read
})
```

`component(name)` is the only way the tree gets a component. An unknown name raises
`module '<name>' not found:`, and the loader reports that for that row only.

## Row primitives

| Primitive | Returns |
|---|---|
| `prims.rows()` | The rows for the current definitions and config, in load order, then one disabled row per definition the config turned off. |
| `prims.refused()` | The definitions refused by the static provide check. |
| `prims.profile_record(label, ms)` | Adds one entry to the startup profile (`/profile`, `--startuptime`). The loader records `plugin <name> (<origin>)` with the CPU time of each plugin's `apply`, as the old path does. |

A row is `{ id, name, config, disabled, origin, path, overrides }`. `id` and `name` are the plugin
name. `disabled` is set when `plugins.<name>.enabled = false` or a builtins profile turns the
plugin off. The loader reads `id`, `name`, `config` and `disabled`. It keeps `origin`, `path` and
`overrides` for `dump()`.

A refused definition declares a service another definition already provides. It is never a row:
rows are keyed by plugin name, so a refused row could stand in for the running one. `dump()` lists
refused definitions after the rows, with the reason, and they never run.

## The `loader` service

| Method | Used by | What it does |
|---|---|---|
| `loader:apply_rows(rows)` | startup, `/reload config` | The tree follows `rows`. New rows start. Removed and disabled rows are disposed, and their cleanups are awaited. A row whose config changed updates its fiber with the new config. Other fibers are left alone. A fiber that had failed before the call is updated once with its config. |
| `loader:replace(name, component)` | `/reload <name>` | Replaces the running definition, and its fibers restart on the new one. If the new definition fails to apply, the old one is put back and the future raises the error. |
| `loader:dump()` | `mare plugin tree`, `/plugins` | One item per row: origin, path, overrides, disabled, the fiber state, the services a pending plugin still waits for (`missing`), the services it provides and its effect count. Then the refused definitions. |

`apply_rows` and `replace` return a cordis Future; `:await()` it.

`missing` is read from each fiber's `_store`, the field cordis-lua's `api-core.md` lists under
"Diagnostics". This is a read-only diagnostic. Nothing else in mare reads `_store`.

## `mare plugin tree`

`mare plugin tree` prints `loader:dump()` as one line per plugin: id, origin, status (the fiber state,
`disabled` or `refused`) and details. `mare plugin tree --json` prints the dump itself.
