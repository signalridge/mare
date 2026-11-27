---@meta mare.test
-- LuaLS definitions: spec organization and assertions (`mare.test` / `require("mare.test")`).

---@class mare.test
mare.test = {}

---@class mare.test.expect
mare.test.expect = {}

--- Assert that `actual` equals `expected` (deep structural equality for tables).
---@param actual any
---@param expected any
---@param msg? string
function mare.test.expect.eq(actual, expected, msg) end

--- Assert that `actual` does not equal `unexpected` (deep structural equality for tables).
---@param actual any
---@param unexpected any
---@param msg? string
function mare.test.expect.ne(actual, unexpected, msg) end

--- Assert that `value` is truthy (neither `nil` nor `false`).
---@param value any
---@param msg? string
function mare.test.expect.truthy(value, msg) end

--- Assert that `value` is falsy (`nil` or `false`).
---@param value any
---@param msg? string
function mare.test.expect.falsy(value, msg) end

--- Assert that calling `fn` raises an error, optionally matching Lua pattern `pattern`.
---@param fn fun()
---@param pattern? string
---@param msg? string
function mare.test.expect.error(fn, pattern, msg) end

--- Assert that string `str` matches Lua pattern `pattern`.
---@param str string
---@param pattern string
---@param msg? string
function mare.test.expect.match(str, pattern, msg) end

--- Assert that `value` matches the stored snapshot beside the spec file (`snapshots/<spec_stem>__<name>.snap`).
--- When `name` is omitted, it is derived from the current test case name.
---@param value any
---@param name? string
function mare.test.expect.snapshot(value, name) end

--- Assert that every loaded plugin in `host` has settled cleanly (all loaded plugins are `"ACTIVE"` with no failures or pending dependencies).
---@param host mare.TestHost
---@param msg? string
function mare.test.expect.settled(host, msg) end

---@class mare.TestHostOptions
---@field builtins? string[] built-in plugin names to preload into this isolated test host
---@field config? table<string, table> per-plugin configuration overrides keyed by plugin name

---@class mare.TestHost
local TestHost = {}

--- Load a plugin definition table or built-in plugin name into the test host and wait for it to settle.
---@param def mare.PluginDef|string
---@param cfg? table
---@return mare.TestHost
function TestHost:load(def, cfg) end

--- Provide a service implementation under `name` through full contract admission and runtime guards.
--- Pass `{ contract = ... }` in `opts` when providing a service without a built-in contract.
--- Returns a disposer function that withdraws the service and settles dependents.
---@param name string
---@param impl table
---@param opts? mare.ServiceProvideOptions
---@return fun()
function TestHost:provide(name, impl, opts) end

--- Return the live service proxy for `name` from this test host, or `nil` if unavailable.
---@param name string
---@return any
function TestHost:get(name) end

--- Reload a loaded plugin by name (optionally with a replacement definition table and/or new config).
--- If the replacement fails to apply, rolls back to the previous active version.
---@param name string
---@param def? mare.PluginDef|string
---@param cfg? table
---@return boolean ok
---@return string? err
function TestHost:reload(name, def, cfg) end

--- Return the current loader state of plugin `name` (`"ACTIVE"`, `"PENDING"`, `"LOADING"`, `"UNLOADING"`, `"FAILED"`, `"DISPOSED"`, `"NONE"`), or `nil` if unknown.
---@param name string
---@return string?
function TestHost:state(name) end

--- Return the last apply/load error recorded for plugin `name`, or `nil`.
---@param name string
---@return string?
function TestHost:error(name) end

--- Unload all plugins and services and close this isolated test host. Idempotent.
function TestHost:dispose() end

--- Create an isolated real plugin host (Cordis tree, loader, event bus, and contract admission).
--- Any test host not explicitly disposed is disposed automatically when its test case finishes.
---@param opts? mare.TestHostOptions
---@return mare.TestHost
function mare.test.host(opts) end

--- Group related spec cases under a named suite (may be nested).
---@param name string
---@param fn fun()
function mare.test.describe(name, fn) end

--- Define a single spec test case. Cases within the same spec file share an isolated file-level Lua environment (use `before_each` / `after_each` to reset shared state).
---@param name string
---@param fn fun()
function mare.test.it(name, fn) end

--- Register a hook that runs before each test case in the current suite and its nested suites.
---@param fn fun()
function mare.test.before_each(fn) end

--- Register a hook that runs after each test case in the current suite and its nested suites.
---@param fn fun()
function mare.test.after_each(fn) end

return mare.test
