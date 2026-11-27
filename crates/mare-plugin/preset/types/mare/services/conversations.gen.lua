---@meta _
-- GENERATED from types/mare/services/contracts/conversations.lua (service `conversations`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.ConversationsService
local M = {}

--- Cancel the active side conversation turn without closing it.
---@return boolean
function M:cancel() end

--- Cancel and discard the active side conversation.
---@return boolean|nil
---@return string?
function M:close() end

--- Return the open side conversation snapshot, or nil when closed.
---@return mare.ConversationInfo|nil
function M:info() end

--- Start a side conversation from a committed snapshot of the current session.
---@param opts? {before_item_id?: string, effort?: string, model?: string}
---@return mare.ConversationInfo|nil
---@return string?
function M:start(opts) end

--- Toggle focus between the main conversation and an open side conversation.
---@return mare.ConversationInfo|nil
---@return string?
function M:toggle() end
