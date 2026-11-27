---@meta _
-- GENERATED from types/mare/services/contracts/copy.lua (service `copy`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class CopyService
local M = {}

--- Re-pad every GFM table in Markdown text so its columns line up.
---@param md string
---@return string
function M:align_tables(md) end

--- Extract fenced code blocks from Markdown text, in order.
---@param md string
---@return {code: string, first: string, lang: string}[]
function M:blocks(md) end

--- Copy the Nth-last response (default 1); opens a code-block picker when code blocks are present.
---@param n? integer
---@return {bytes: integer, error?: string, method?: string, ok: boolean, verified?: boolean, [string]: any}|string|nil
function M:last(n) end

--- Markdown source of the Nth-last assistant response (1 = last), or nil.
---@param n? integer
---@return string|nil
function M:response(n) end

--- Copy text to the clipboard, reporting the path used, and emit `copy/completed`.
---@param text string
---@param target? string
---@return {bytes: integer, error?: string, method?: string, ok: boolean, verified?: boolean, [string]: any}
function M:text(text, target) end

--- Write text to `mare-copy-<time>.<ext>` in the working directory and return the path, or nil on error.
---@param text string
---@param lang? string
---@return string|nil
function M:write(text, lang) end
