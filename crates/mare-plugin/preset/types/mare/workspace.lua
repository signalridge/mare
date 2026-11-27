---@meta _
-- LuaLS definitions: workspace directories (entry: types/mare.lua)

---@class mare.workspace
mare.workspace = {}
--- Return the active workspace directories (primary root first).
---@return string[]
function mare.workspace.dirs() end
--- Validate a candidate additional workspace directory (existing physical absolute directory,
--- no protected-path or deny_read overlap, within the active sandbox ceiling). Returns the
--- canonical path, or `nil, err`.
---@param path string
---@param peers? string[] optional sibling candidate roots to check pairwise protected-path isolation against
---@return string|nil canon, string|nil err
function mare.workspace.check(path, peers) end
--- Atomically replace the additional workspace directories (the primary root stays first).
--- Validates every candidate root (same rules as `add`) before mutating; rejects all-or-nothing.
---@param dirs string[]
---@return string[]
function mare.workspace.set_dirs(dirs) end
--- Add a workspace directory (resolved via `mare.fs.resolve`); returns true if newly added.
---@param dir string
---@return boolean
function mare.workspace.add(dir) end
--- Remove an added workspace directory (the primary root cannot be removed).
---@param dir string
---@return boolean
function mare.workspace.remove(dir) end
--- Whether path `p` lies inside any active workspace directory.
---@param p string
---@return boolean
function mare.workspace.contains(p) end
