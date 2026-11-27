# The boot fragment and the `task` host plugin

`crates/mare-kernel/lua/mare_kernel/boot.lua` builds the cordis host on `mare.task` and the root
context, uses the host plugins (`task`, `loader`), and turns each plugin definition into a cordis
component (`component.lua`: its owner record, its `lua/` directory, the apply's outcome for the
failure and reload reports). Types: `types/mare/host/kernel.lua`.

## Primitives

The boot fragment receives `prims` from Rust (`lua_env/cordis_host.rs`). Host plugins get the
typed part as their config. Not listed (the boot fragment's own): `capture`, `spawn_root`, and
`run_root`.

| entry | |
|---|---|
| `handler_failed(plugin, what, err)` | report a plugin callback's error: problem line, log, `kernel/error` |
| `unload_modules(name)` | drop the plugin's modules from `package.loaded` (called when its fiber unloads) |
| `mark_active(name, owner \| nil, config?)` | record the active plugin instance and its effective `config` table while its fiber runs, or clear it when `owner` is `nil` |

The owner, event, definition and reload tables are in [the prims types](https://github.com/signalridge/mare/blob/main/crates/mare-plugin/preset/types/mare/host/prims.lua). The rows are in [the loader host plugin](host-loader.md).

## Start and reload

`booted.start()` and `booted.reload(name, targets)` run in the host's own context
(`prims.run_root`). A reload triggered by a restricted peer or from inside a tool does not pass
that caller's grant to the fibers it rebuilds.

- `reload(name)` of a running plugin: `loader:replace`. If the new definition fails, the old
  one is put back.
- Otherwise, the rows leave the tree one at a time, last row first: first the rows that are
  gone, then `targets`. Then the full row list builds each target again from its new definition
  and config.

## `task`

`ctx:after(ms, fn)`, `ctx:every(ms, fn)` and `ctx:spawn(fn, ...)` start tasks owned by the
caller's fiber; they are aborted when it unloads. A plugin that uses them declares
`inject = { "task" }`.

## `kernel`

The contracted service `kernel` (`mare_kernel.plugins.kernel`, a `mare.Service`). It replaces the
old Ctx methods `plugins`, `problems`, `is_builtin`, `provides`, `reload` and `reload_barrier`; a
plugin that uses it declares `inject = { "kernel" }`.
`kernel:reload(name?)` takes the full reload path (Rust prepares the definitions, then the tree
switches), detached from the caller, which the reload may dispose.
