---@meta _
-- GENERATED from types/mare/services/contracts/changes.lua (service `changes`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class ChangesService
local M = {}

--- Each turn's before/after file contents and diff stats, oldest first.
---@return mare.ChangeTurn[]
function M:diffs() end

--- Restore workspace files to their snapshot before `turn`.
---@param turn string
---@param opts? {force?: boolean}
---@return table<string, any>|nil
function M:rewind(turn, opts) end

--- Recorded journal turns with modified files, oldest first.
---@return mare.ChangeTurn[]
function M:turns() end
