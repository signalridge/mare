---@meta _
-- GENERATED from types/mare/services/contracts/options.lua (service `options`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.OptionsService
local M = {}

--- Read effective options (or descriptors when opts.describe is true) with live service overrides and shared sensitive-path redaction.
---@param keys? string[]
---@param opts? {describe?: boolean}
---@return table<string, any>
function M:get(keys, opts) end

--- Validate and transactionally commit a batch of option updates (core options, llm model/effort, and plugins.<plugin>.<option>) with full rollback on refusal.
---@param values table<string, any>
---@param opts? {validate_only?: boolean}
---@return table
function M:set(values, opts) end
