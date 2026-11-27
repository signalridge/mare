---@meta mare_lib.table_revision
-- Public pure-Lua table graph observations, usable by every plugin through the reserved
-- shared module loader. The helper holds references; it is not a serialization format.

---@class mare.TableRevision
--- Opaque in-memory observation of raw table entries, their identities and visible
--- metatables, including table keys and cycles. Do not modify or serialize it.

---@class mare_lib.table_revision
local M = {}

--- Observe the reachable table graph without invoking indexing/pairs metamethods.
--- Non-table roots produce an empty observation. Equality is reference/raw equality;
--- opaque userdata contents and hidden protected metatables are not traversed.
---@param root any
---@return mare.TableRevision
function M.snapshot(root) end

--- True if every observed table still has the same raw keys/values and visible metatable.
---@param revision mare.TableRevision
---@return boolean
function M.unchanged(revision) end

return M
