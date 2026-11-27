---@meta _
-- GENERATED from types/mare/services/contracts/subagent.lua (service `subagent`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.SubagentService
local M = {}

--- Request cancellation of a subagent and its descendants.
---@param subagent_id string
---@return table
function M:cancel(subagent_id) end

--- Close admission and wait for the current session's subagents and completion notices.
---@return table
function M:drain() end

--- Get a subagent by its subagent_id (or unique name).
---@param subagent_id string
---@return {ended_at_ms?: integer, error?: {code: string, data?: any, message: string}, error_kind?: string, label?: string, name?: string, parent_session_id?: string, run_id?: string, seq?: integer, started_at_ms: integer, status: "running"|"completed"|"failed"|"cancelled"|"interrupted", subagent_id: string, task?: string}
function M:get(subagent_id) end

--- List subagents started in this host (optionally paginated by cursor and limit).
---@param opts? {cursor?: string, limit?: integer}
---@return {data: ({ended_at_ms?: integer, error?: {code: string, data?: any, message: string}, error_kind?: string, label?: string, name?: string, parent_session_id?: string, run_id?: string, seq?: integer, started_at_ms: integer, status: "running"|"completed"|"failed"|"cancelled"|"interrupted", subagent_id: string, task?: string})[], next_cursor?: string}
function M:list(opts) end

--- List current subagent type definitions in bytewise name order.
---@return {data: {description: string, disallowed_tools?: string[], effort?: string, model?: string, name: string, system?: string, tools?: string[], when_to_use?: string}[]}
function M:list_types() end

--- Register a session-owned subagent type. Names use ASCII letters, digits, underscore, hyphen or dot; tool lists are dense, nonempty-string arrays without duplicates.
---@param definition {description: string, disallowed_tools?: string[], effort?: string, model?: string, name: string, system?: string, tools?: string[], when_to_use?: string}
---@param opts? {replace?: boolean}
---@return fun() dispose releases this registration
function M:register_type(definition, opts) end

--- Start an isolated child subagent and return its subagent info.
---@param opts {config?: table, cwd?: string, effort?: string, fork?: ("none"|"all")|integer, label?: string, messages?: table[], model?: string, name?: string, output_schema?: table, params?: table, persona?: string, phase?: string, prompt: string, run_id?: string, sections?: table[], seq?: integer, system?: string, task?: string, tools?: string[], type?: string}
---@return {ended_at_ms?: integer, error?: {code: string, data?: any, message: string}, error_kind?: string, label?: string, name?: string, parent_session_id?: string, run_id?: string, seq?: integer, started_at_ms: integer, status: "running"|"completed"|"failed"|"cancelled"|"interrupted", subagent_id: string, task?: string}
function M:start(opts) end

--- Deliver steering input to a running subagent.
---@param subagent_id string
---@param text string
---@return table
function M:steer(subagent_id, text) end

--- Wait for a subagent to finish (non-consuming; timeout or cancellation does not cancel the child).
---@param subagent_id string
---@param opts? {timeout_ms?: integer}
---@return {error?: {code: string, data?: any, message: string}, error_kind?: string, messages?: table[], status: "running"|"completed"|"failed"|"cancelled"|"interrupted", subagent_id: string, text?: string, timed_out: boolean, usage?: table}
function M:wait(subagent_id, opts) end
