---@meta _
-- GENERATED from types/mare/services/contracts/cells.lua (service `cells`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.CellsService
local M = {}

--- Cancel an active cell owned by this caller.
---@param cell_id string
---@return table
function M:cancel(cell_id) end

--- Status of the caller's cell environment.
---@return {retains_state: boolean, running: integer, store_durable: boolean, stored_keys: integer}
function M:get() end

--- List cells owned by this caller.
---@param opts? {cursor?: string, limit?: integer}
---@return {data: ({cell_id: string, status: "running"|"completed"|"failed"|"cancelled"})[], next_cursor?: string}
function M:list(opts) end

--- Discover tools currently callable from code cells, including the MCP codemode tier.
---@param opts? {cursor?: string, limit?: integer}
---@return {data: {declaration: string, description: string, name: string, output_schema: table, parameters: table}[], next_cursor?: string}
function M:list_tools(opts) end

--- Cancel active cells owned by this caller and release caller-owned retained VM state.
---@return table
function M:reset() end

--- Start an asynchronous script cell in this caller's scope and wait up to yield_time_ms for initial output or completion. Lua callers may pass `cancel` / `wait_cancel` returning their cancel token.
---@param code string
---@param opts? {capabilities?: string[], cell_id?: string, max_output_tokens?: integer, state_owner?: string, timeout_ms?: integer|boolean, yield_time_ms?: integer, cancel?: (fun(): mare.Token|nil), wait_cancel?: (fun(): mare.Token|nil)}
---@return mare.CellResult
function M:start(code, opts) end

--- Wait for an active or completed cell owned by this caller, consuming newly emitted output. terminate cancels the cell and waits up to 5 s. Lua callers may pass `cancel` returning their cancel token.
---@param cell_id string
---@param opts? {max_output_tokens?: integer, terminate?: boolean, yield_time_ms?: integer, cancel?: (fun(): mare.Token|nil)}
---@return mare.CellResult
function M:wait(cell_id, opts) end
