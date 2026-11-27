---@meta _
-- LuaLS definitions for the `loader` host plugin and the row primitives it is given (host
-- plugins use only typed, documented primitives). These tables are
-- not globals and not modules: the boot fragment (boot.lua) passes them to the host plugin as its
-- config. Doc: docs/en/extend/host-loader.md.

--- One row of the plugin tree, built by the kernel from the merged definitions and the config
--- (crates/mare-kernel/src/plugins/rows.rs). `origin`, `path` and `overrides` are kept on the
--- entry for `dump()`; cordis.loader itself reads `id`, `name`, `config`, `disabled`.
---@class mare.host.Row
---@field id string the plugin name (rows are keyed by it)
---@field name string the plugin name, which `component(name)` resolves
---@field config? table the plugin's options from the current config
---@field disabled? true turned off: `plugins.<name>.enabled = false` or a builtins profile
---@field origin? "builtin"|"package"|"user"|"project" where the definition comes from
---@field path? string the definition's file
---@field overrides? "builtin"|"package"|"user"|"project" the origin of the same-name definition it replaced
---@field fingerprint? string what a full reload compares to tell whether the definition changed
---@field wave? integer best-effort static `provide -> inject` startup wave (0 = no in-tree provider dependencies); rows stay in load order

--- A definition refused by the static provide check (it declares a service another definition
--- already provides). Never a row: it is listed by `dump()` after the rows and never runs.
---@class mare.host.RefusedDefinition
---@field id string
---@field origin? "builtin"|"package"|"user"|"project"
---@field path? string
---@field refused string why, naming the definition that keeps the service and its file

--- The row primitives the boot fragment hands to the loader host plugin.
---@class mare.host.RowPrims
--- The rows for the current definitions and config, in load order; then one disabled row per
--- definition the config turned off. Computed when called.
---@field rows fun(): mare.host.Row[]
--- The definitions refused by the static provide check. Computed when called.
---@field refused fun(): mare.host.RefusedDefinition[]
--- Add one entry to the startup profile (`/profile`, `--startuptime`): records
--- `plugin <name> (<origin>)` via `os.clock()` by default; when startup profiling
--- (`--startuptime` / `--profile-startup`) also provides `measure_apply`, appends
--- `[sync|yielded, req=<k>]` with the plugin's own synchronous apply time.
---@field profile_record? fun(label: string, ms: number)
--- Present only when startup profiling (`--startuptime` / `--profile-startup`) is enabled:
--- runs a plugin's `apply` inside a timed scope returning `(busy_ms, yielded, req_count, ...)`.
---@field measure_apply? fun(fn: function, ...): number, boolean, integer, ...
--- Monotonic O(1) counter incremented whenever an `owner.enter` future yields (`Poll::Pending`);
--- read before and after `apply` for zero-overhead yield detection across all plugin origins.
---@field poll_count? fun(): integer

--- The loader host plugin's config (`root:use(require("mare_kernel.plugins.loader"), config)`).
---@class mare.host.LoaderConfig
--- The cordis component for a plugin name (its lua/ dir registered first). The only way the tree
--- gets a component; an unknown name raises `module '<name>' not found:`, which fails that row only.
--- Returns `(component, origin?, measured?)` where `measured = true` means `component.lua` already
--- recorded the startup profile entry for `def.apply`.
---@field component fun(name: string): table, string?, boolean?
---@field prims? mare.host.RowPrims `refused` (for `dump()`) and `profile_record` are read

--- One item of `dump()`: a row with its fiber's state, or a refused definition.
---@class mare.host.DumpItem
---@field id string
---@field origin? string
---@field path? string
---@field overrides? string
---@field disabled boolean
---@field refused? string set for a refused definition (then `disabled` is true and there is no state)
---@field state? "PENDING"|"LOADING"|"ACTIVE"|"FAILED"|"DISPOSED"|"NONE" the fiber's state; `NONE`: its component could not be imported; absent when disabled
---@field missing? string[] for a PENDING plugin: the injected services with no provider yet
---@field provides? string[] the services the fiber provides
---@field effects? integer how many effects (cleanups) the fiber holds

--- The `loader` service: a cordis-lua `Loader` (cordis.loader) that the kernel drives.
---@class mare.host.Loader
local Loader = {}

--- The whole tree follows `rows`: new rows start, removed and disabled rows are disposed (their
--- cleanups awaited), a row whose config changed updates its fiber with the new config, the other
--- fibers are left alone; a fiber that had failed before the call is updated once with its config.
--- Startup and `/reload config`.
---@param rows mare.host.Row[]
---@return table future a cordis Future: `:await()` it
function Loader:apply_rows(rows) end

--- `/reload plugin <name>`: the running plugin's definition is replaced by `component` (its fibers
--- restart on it); if the new definition fails to apply, the old one is put back and the error is
--- raised by the future.
---@param name string
---@param component table
---@return table future a cordis Future: `:await()` it
function Loader:replace(name, component) end

--- The state of every row, then the refused definitions (`mare plugin tree`, `/plugins`).
--- Reads each fiber's `_store` for `missing`, as cordis-lua's api-core.md "Diagnostics" documents.
---@param opts? { effects?: boolean } pass `{ effects = false }` to skip counting `item.effects`
---@return mare.host.DumpItem[]
function Loader:dump(opts) end
