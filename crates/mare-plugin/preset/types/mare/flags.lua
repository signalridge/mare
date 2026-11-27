---@meta
-- LuaLS definitions: CLI extension flags (entry: types/mare.lua)

---@class mare.FlagOpts
---@field type "boolean"|"string"
---@field description? string
---@field default? boolean|string

---@class mare.FlagInfo
---@field name string
---@field type "boolean"|"string"
---@field description? string
---@field default? boolean|string
---@field value? boolean|string
---@field source? string

--- Extension CLI flags (`init.lua` registration or declarative `PluginDef.flags`).
---@class mare.flags
mare.flags = {}

--- Register a CLI flag (`--<name>` / `--<name>=<val>`).
---@param name string
---@param opts mare.FlagOpts
function mare.flags.register(name, opts) end

--- Read a registered CLI flag value (CLI override -> default -> nil; nil if unregistered).
---@param name string
---@return boolean|string|nil
function mare.flags.get(name) end

--- List all registered CLI flags and their resolved values.
---@return mare.FlagInfo[]
function mare.flags.list() end
