---@meta _
-- GENERATED from types/mare/services/contracts/spill.lua (service `spill`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.SpillService
local M = {}

--- Invalidate and delete a session-owned spill object.
---@param spill_id string
---@return table
function M:delete(spill_id) end

--- Delete a spill object owned by an explicit `session_id` (Lua-only).
---@param session_id string
---@param spill_id string
---@return boolean
function M:delete_owned(session_id, spill_id) end

--- Registered spill storage backends, with which one is currently selected.
---@return {data: {name: string, selected: boolean}[]}
function M:list_backends() end

--- Read a UTF-8 text page (default) or base64 byte page (`encoding = "base64"`) from a session-owned spill object.
---@param spill_id string
---@param opts? {encoding?: "text"|"base64", limit?: integer, offset?: integer}
---@return {content?: string, data_base64?: string, next_offset?: integer, size?: integer}
function M:read(spill_id, opts) end

--- Read raw binary bytes owned by an explicit `session_id` (Lua-only; used by UI image materializers).
---@param session_id string
---@param spill_id string
---@param opts? {offset?: integer, limit?: integer}
---@return {bytes: string, next_offset?: integer, size: integer, [string]: any}
function M:read_bytes(session_id, spill_id, opts) end

--- Read UTF-8 text owned by an explicit `session_id` (Lua-only).
---@param session_id string
---@param spill_id string
---@param opts? {offset?: integer, limit?: integer}
---@return {content: string, next_offset?: integer, [string]: any}
function M:read_owned(session_id, spill_id, opts) end

--- Register a named spill backend; removed automatically on disposal.
---@param name string
---@param backend mare.LocalSpill
---@return fun() dispose releases this registration
function M:register(name, backend) end

--- Persist UTF-8 text (`content`) or strict padded base64 bytes (`data_base64`, max 8 MiB decoded) in the active session's spill storage.
---@param opts? {backend?: string, content?: string, data_base64?: string}
---@return {size: integer, spill_id: string}
function M:save(opts) end

--- Save raw binary bytes (max 8 MiB) owned by an explicit `session_id` (Lua-only; includes host `path`).
---@param session_id string
---@param bytes string
---@param opts? (mare.SpillSaveOpts|{backend?: string})
---@return mare.SpillServiceRef
function M:save_bytes(session_id, bytes, opts) end

--- Save UTF-8 text owned by an explicit `session_id` (Lua-only; includes host `path`).
---@param session_id string
---@param content string
---@param opts? (mare.SpillSaveOpts|{backend?: string})
---@return mare.SpillServiceRef
function M:save_owned(session_id, content, opts) end
