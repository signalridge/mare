---@meta _
-- GENERATED from types/mare/services/contracts/hooks.lua (service `hooks`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.HooksService
local M = {}

--- Subscribe `handle(event, mode, args)` to a bus event or policy decision point. Returns a registration removed on release or client disconnect. `prepend = true` inserts before earlier listeners (`prepend = false` keeps append order); `timeout_ms` requires `>= 1` and is clamped to the host policy timeout ceiling for remote callers.
---@param event string
---@param opts? {prepend?: boolean, timeout_ms?: integer, handle?: (fun(event: string, mode: string, args: any[]): any)}
---@return fun() dispose releases this registration
function M:subscribe(event, opts) end
