---@meta _
-- The kernel capability tables a host plugin receives as `config.prims` from the boot fragment.
-- Public primitives for host plugins only: never a global, never
-- requirable. `prims.spawn_root` and `prims.capture` are the boot fragment's and the library's and
-- are not listed here. This file: the owner, event, definition and reload tables;
-- `prims.contracts` and `prims.rows` / `prims.refused` are typed by
-- their owners.

--- Attribution only (no lifecycle): the fiber that opened an owner closes it.
---@class mare.host.Owners
local Owners = {}
--- A record that attributes listeners, problems, timing and RPC registration ids to plugin `name`.
---@param name string
---@param origin "builtin"|"package"|"user"|"project"|"root"
---@return integer id
function Owners.open(name, origin) end
--- Drop the record and the listeners / local declarations still attributed to it. Idempotent.
---@param id integer
function Owners.close(id) end
--- The owner the running work acts for, or nil for the host.
---@return integer|nil
function Owners.current() end
--- Run `fn(...)` as owner `id`'s work (its emits and logs are attributed to it).
---@async
---@param id integer
---@param fn async fun(...): ...
---@return ...
function Owners.enter(id, fn, ...) end

--- The kernel's event dispatch, the bus host plugin's way into Rust (mare_kernel/plugins/bus.lua).
---@class mare.host.Events
local Events = {}
--- Dispatch in one of the bus's modes (emit, emit_checked, bail, waterfall, waterfall_checked; the
--- waterfall forms take the value first). The emitter is `emitter()`, never an argument.
---@async
---@param mode string
---@param event string
---@return ...
function Events.dispatch(mode, event, ...) end
--- The owner id the running work acts for, or "host" (Rust, root).
---@return integer|"host"
function Events.emitter() end
---@return integer
function Events.policy_epoch() end

---@class mare.host.Reload
local Reload = {}
--- Wait until no plugin reload is in progress.
---@async
function Reload.barrier() end
---@return integer
function Reload.generation() end

--- `config.prims` as a host plugin receives it.
---@class mare.host.Prims
---@field owner mare.host.Owners
---@field events mare.host.Events
---@field reload mare.host.Reload
---@field barrier fun(service: string, method: string) the poisoned-session check for a service method (raises; diagnostics allowed)
---@field definition fun(name: string): table|nil the evaluated definition table
---@field module_dir fun(name: string) register the plugin's `lua/` modules for require (idempotent)
