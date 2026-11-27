---@meta mare_lib.schema_sanitize

---@class mare_lib.schema_sanitize
local M = {}

---@param dialect? string
---@param base_url? string
---@param model_id? string
---@param opts? table
---@return table
function M.detect_dialect(dialect, base_url, model_id, opts) end

---@param pattern any
---@return any
function M.unicode_null_escape(pattern) end

---@param pattern any
---@return any
function M.ecma_class_for_rust(pattern) end

---@param schema any
---@param dialect? table|string
---@param base_url? string
---@param model_id? string
---@param opts? table
---@return any
function M.sanitize(schema, dialect, base_url, model_id, opts) end

return M
