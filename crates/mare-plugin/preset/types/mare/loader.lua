---@meta _
-- LuaLS definitions: reloads, the request gate, deprecation and the bytecode cache (entry: types/mare.lua)

--- plugins started, plugins of removed packages disposed, everything else reloaded (a plugin
--- whose new definition fails keeps the old one), packages' config() again. Also emitted as
--- `reload/completed`.
---@async
---@return {added: string[], removed: string[], replaced: string[], reloaded: string[], notices: string[]}
function mare.reload() end
--- The host's reload state as an opaque string: it changes with every config or plugin reload
--- (and while one is running or incomplete). Compare it with an earlier value to tell whether a
--- cached decision (e.g. a tool approval) predates a reload. `"closed"` once the host is gone.
---@return string
function mare.reload_version() end
--- Bytecode cache (Neovim `vim.loader`, ~/.cache/mare/luac).
---@class mare.loader
mare.loader = {}
--- Drop the whole cache, or one source file's entry. Returns how many entries were removed.
---@param path? string
---@return integer
function mare.loader.reset(path) end
--- Drop entries for another mare version or a changed / missing source (also done at most
--- daily on startup).
---@return integer
function mare.loader.prune() end
