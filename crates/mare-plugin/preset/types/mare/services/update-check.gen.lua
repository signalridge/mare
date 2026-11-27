---@meta _
-- GENERATED from types/mare/services/contracts/update_check.lua (service `update-check`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class UpdateCheckService
local M = {}

--- Query the configured release endpoint for a newer mare version; returns `{latest, current, newer}` or `nil, reason`.
---@return {latest: string, current: string, newer: boolean}|nil
---@return string|nil
function M:check() end
