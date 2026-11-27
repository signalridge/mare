---@meta _
-- GENERATED from types/mare/services/contracts/images.lua (service `images`): do not edit.
-- Regenerate: UPDATE_GENERATED=1 cargo test -p mare-cli generated_luals_are_current

---@class ImagesService
local M = {}

--- Render image blocks from a message or tool-result content list when an image protocol is active.
---@param content table<string, any>[]
---@return table|nil
function M:show(content) end
