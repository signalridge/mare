---@meta _
-- LuaLS definitions: Lua standard-library additions (entry: types/mare.lua)

-- std additions installed by the core
---@generic T
---@param t T[]
---@param x T
---@return integer|nil
function table.find(t, x) end
---@generic T: table
---@param t T
---@return T
function table.copy(t) end
