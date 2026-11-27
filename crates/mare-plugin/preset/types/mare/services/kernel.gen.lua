---@meta _
-- GENERATED from types/mare/services/contracts/kernel.lua (service `kernel`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.host.KernelService
local M = {}

--- begin an atomic session identity switch: reserve target `session_id` (if given) and revoke session-only trust before publish; `handle:commit(session_id)` automatically marks any active session export call as committed
---@param opts? {session_id?: string}
---@return mare.host.SessionSwitchHandle
function M:begin_session_switch(opts) end

--- return the opaque caller identifier of the running request
---@return string
function M:get_caller() end

--- whether plugin `name` is a built-in
---@param name string
---@return boolean
function M:is_builtin(name) end

--- list currently open sessions across managed runtimes (or this standalone host)
---@return {session_epoch?: string, session_id: string}[]
function M:list_sessions() end

--- mark the running service export call as past its commit boundary
---@return table<string, any>
function M:mark_call_committed() end

--- plugin `name`'s row in the plugin list, 0 if it has none
---@param name string
---@return integer
function M:plugin_row(name) end

--- the plugin rows: name, origin, active, overrides
---@return { name: string, origin: string, active: boolean, overrides: string|nil }[]
function M:plugins() end

--- the current problem lines
---@return string[]
function M:problems() end

--- whether the caller's fiber provides `name`
---@param name string
---@return boolean
function M:provides(name) end

--- reload plugin `name`, or all plugins
---@param name? string
---@return string[]
function M:reload(name) end

--- wait until no reload is running
---@return table<string, any>
function M:reload_barrier() end

--- the running (or latest) reload's generation: one more per plugin reload and per config row update
---@return integer
function M:reload_generation() end

--- acquire the exclusive per-host request/reload lease bound to the caller owner lifetime
---@param cancel mare.Token|{ cancel?: mare.Token }|nil
---@return mare.host.RequestLease|nil, string|nil
function M:request_lease(cancel) end

--- whether this host orchestrates a single session runtime ("single") or multiple concurrent runtimes ("multi")
---@return "single"|"multi"
function M:session_capacity() end

--- the current event stream session epoch
---@return string
function M:session_epoch() end

--- the plugins stuck loading, unloading or waiting for `stall_ms` or longer; observed, never aborted
---@return ({effects: string[], missing: string[], ms: integer, plugin: string, state: "LOADING"|"UNLOADING"|"PENDING", summary: string})[]
function M:stalled() end
