---@meta _
-- GENERATED from types/mare/services/contracts/workspace.lua (service `workspace`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.WorkspaceService
local M = {}

--- Return the canonical workspace root, trust state, and workspace directory list.
---@return {dirs: string[], root: string, trusted: boolean}
function M:get() end

--- Update project trust for this workspace across all active sessions attached to it.
---@param trusted boolean
---@param save? boolean
---@return table
function M:set_trust(trusted, save) end
