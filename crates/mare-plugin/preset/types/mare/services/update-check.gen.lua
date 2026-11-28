---@meta _
-- GENERATED from types/mare/services/contracts/update_check.lua (service `update-check`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class UpdateCheckService
local M = {}

--- Query the configured release endpoint for a newer mare version; returns `{latest, current, newer, channel?, managed?, command?, manual_hint?, release_url?, policy?}` or `nil, reason`.
---@return {latest: string, current: string, newer: boolean, channel?: string, managed?: boolean, command?: string, manual_hint?: string, release_url?: string, policy?: string}|nil
---@return string|nil
function M:check() end
