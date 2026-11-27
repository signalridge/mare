---@meta _
-- GENERATED from types/mare/services/contracts/message.lua (service `message`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.MessageService
local M = {}

--- Append a custom message that enters model context while honoring display, delivery, and optional `request_id` idempotency.
---@param opts {content: string|any[], created_at_ms?: integer, delivery?: {deliver_as?: "steer"|"follow_up"|"next_turn", trigger_turn?: boolean, wait?: boolean}, details?: any, display?: boolean, request_id?: string, type: string}
---@return {duplicate?: boolean, message: table, result?: table, status: "appended"|"queued"|"deferred"|"triggered"}
function M:append(opts) end

--- Look up the normalized active spec for a registered custom message type (Lua-only).
---@param custom_type string
---@return mare.CustomMessageSpec?
function M:get_type(custom_type) end

--- List currently registered custom message types in sorted name order.
---@return {data: {type: string}[]}
function M:list_types() end

--- Register a custom message type with optional validation and render callbacks; returns a disposer (`registration_id` over RPC).
---@param type string
---@param opts? {compact?: (fun(msg: table, opts: {expanded: boolean}): mare.Node|nil), expanded?: (fun(msg: table, opts: {expanded: boolean}): mare.Node|nil), render?: (fun(msg: table, opts: {expanded: boolean}): mare.Node|nil), validate?: (fun(msg: mare.CustomMessageInput): boolean|nil, string|nil)}
---@return fun() dispose releases this registration
function M:register_type(type, opts) end
