---@meta _
-- GENERATED from types/mare/services/contracts/usage.lua (service `usage`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class mare.UsageService
local M = {}

--- Tokens occupying the context window after the last request.
---@return integer
function M:context_tokens() end

--- Cost as text: "$0.1234", "subscription, not billed", or both.
---@param format? string
---@return string
function M:cost_text(format) end

--- Totals, context tokens and rate limits of the session in one object.
---@return {context_tokens: integer, rate_limits: table<string, {credits?: string, limit_name?: string, primary?: {reset_at_ms?: integer, used_percent: number, window_minutes?: number, [string]: any}, reached?: string, received_at_ms: integer, secondary?: {reset_at_ms?: integer, used_percent: number, window_minutes?: number, [string]: any}, [string]: any}>, totals: {cache_read: integer, cache_write: integer, cache_write_1h?: integer, cost: number, cost_unknown?: boolean, input: integer, last_cache_read: integer, last_context: integer, output: integer, reasoning?: integer, requests: integer, side_requests: integer, subscription_requests: integer}}
function M:get() end

--- Subscription quota as display lines, one per window (/status, the statusline).
---@return {gauge?: number, text: string, warning?: boolean}[]
function M:quota_lines() end

--- Latest subscription quota per provider (the llm/rate-limits event).
---@return table<string, {credits?: string, limit_name?: string, primary?: {reset_at_ms?: integer, used_percent: number, window_minutes?: number, [string]: any}, reached?: string, received_at_ms: integer, secondary?: {reset_at_ms?: integer, used_percent: number, window_minutes?: number, [string]: any}, [string]: any}>
function M:rate_limits() end

--- Update the context token estimate and emit usage/changed; notify = false is silent.
---@param tokens integer
---@param opts? {notify?: boolean}
---@return table
function M:set_context(tokens, opts) end

--- The session's totals: reset by a new session, recounted on resume.
---@return {cache_read: integer, cache_write: integer, cache_write_1h?: integer, cost: number, cost_unknown?: boolean, input: integer, last_cache_read: integer, last_context: integer, output: integer, reasoning?: integer, requests: integer, side_requests: integer, subscription_requests: integer}
function M:totals() end
