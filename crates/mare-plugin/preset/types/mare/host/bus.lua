---@meta _
-- LuaLS types for the `bus` host service (mare_kernel/plugins/bus.lua): a native
-- plugin's entry for mare events. Each call goes through the Rust entry (blocked turns, the event record,
-- the taps) and then the bus, legacy value-passing listeners included. Reached as `ctx.bus` (inject "bus").

---@class mare.BusService
local BusService = {}

--- Notify every listener in order; a failing one is reported and the rest still run.
---@param event string
---@param ... any the event's arguments
function BusService:emit(event, ...) end

--- Like `emit`, but the first failing listener (or one past its timeout_ms) raises.
---@param event string
---@param ... any
function BusService:emit_checked(event, ...) end

--- The first listener's non-nil answer (`false` included), or nil.
---@param event string
---@param ... any
---@return any
function BusService:bail(event, ...) end

--- `value` through the listeners in order (each may pass on a replacement); a failure keeps `value`.
---@param event string
---@param value any
---@param ... any extra arguments every listener gets
---@return any
function BusService:waterfall(event, value, ...) end

--- Like `waterfall`, but a failing listener raises instead of keeping `value`.
---@param event string
---@param value any
---@param ... any
---@return any
function BusService:waterfall_checked(event, value, ...) end
