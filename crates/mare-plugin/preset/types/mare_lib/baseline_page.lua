---@meta mare_lib.baseline_page
-- Shared bounded paging helper for `baseline = true` SPI cuts.

---@alias mare.BaselinePageFn fun(list: table, count: integer, cursor?: string, max_bytes?: integer, limit?: integer): { data: table[], next_cursor?: string }

---@type mare.BaselinePageFn
local function baseline_page(list, count, cursor, max_bytes, limit) end

return baseline_page
