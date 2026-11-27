---@meta _
-- GENERATED from types/mare/services/contracts/query.lua (service `query`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.QueryService
local M = {}

--- Run an in-process query operation inside an ephemeral-read-only sandbox rooted at the active execution context's cwd, or at `mare.cwd` when called directly from Lua without an active `mare.exec.current()`. Lua callers may pass `cancel`, a function returning their cancel token or nil.
---@param operation string
---@param arguments table<string, any>
---@param opts? {provider?: string, cancel?: (fun(): mare.Token|nil)}
---@return {next_cursor?: string, provider?: string, [string]: any}
function M:call(operation, arguments, opts) end

--- Registered query providers and the operations they serve, sorted by name.
---@return {data: ({name: string, operations: string[], priority: integer, revision: string, source: {capability: "in_process"|"opaque"}})[]}
function M:list_providers() end

--- Register capability operations owned by this caller; removed automatically on disposal. Paginated results return next_cursor; subsequent requests receive cursor.
---@param name string
---@param def {priority?: integer, source?: {capability: "in_process"|"opaque"}, operations: table<string, fun(args: table, opts?: {cancel?: mare.Token, provider?: string}): table>}
---@return fun() dispose releases this registration
function M:register(name, def) end

--- Whether a registered in-process provider serves this operation.
---@param operation string
---@return boolean
function M:supports(operation) end
