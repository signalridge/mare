---@meta _
-- GENERATED from types/mare/services/contracts/review.lua (service `review`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class ReviewService
local M = {}

--- Run an isolated read-only code review for `target` and return findings or `nil, err`.
---@param target? table<string, any>
---@return table|nil
---@return string|nil
function M:run(target) end

--- Open the interactive change diff viewer, optionally narrowed to `only`.
---@param only? string
---@return table|nil
function M:viewer(only) end
