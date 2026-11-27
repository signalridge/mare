---@meta _
-- LuaLS types for the contracts primitives. Host
-- plugins get them as `config.prims.contracts`; they are never a global or a module, so plugins
-- reach contracts only through `mare.Service` and `mare.contracts`.

--- What `admit` admitted, for one `export`.
---@class mare.host.ContractHandle

--- Lazy per-method guard handle extracted before `export` and valid across the service's lifetime.
---@class mare.host.GuardHandle

---@class mare.host.Contracts
local Contracts = {}

--- mare.Service's class table, handed once by service.lua: admission's walk over a provider's class
--- chain stops at it, by identity.
---@param class table
function Contracts.base(class) end

--- Contract selection and admission for the running owner (`prims.owner.current()`): the declared
--- contract, or the runtime one in `options.contract`, never replaced by a different one; the value
--- implements every required method, optional ones are functions or absent, a full contract
--- declares every function the value has. Nothing is registered. Raises when refused (the provide fails).
---@param name string
---@param value any
---@param options? {contract?: table, unconfined?: boolean}
---@return mare.host.ContractHandle
function Contracts.admit(name, value, options) end

--- The methods the contract declares for Lua callers (lua, both; not remote-only), before `export`.
---@param handle mare.host.ContractHandle
---@return string[]
function Contracts.methods(handle) end

--- Extract a lazy per-method guard handle from `handle` (before `export`) so `method_guard`
--- can construct guards on first invocation even after `export` consumes `handle`.
---@param handle mare.host.ContractHandle
---@return mare.host.GuardHandle
function Contracts.guard_handle(handle) end

--- One method's guard, for `mare.Service`'s per-method wrappers; nil for an undeclared method.
--- `enter(...)` checks the caller's grant and the arguments, `leave(packed)` settles and checks
--- the result, `fail(err)` turns a raised value into the structured error. A registration's
--- disposer is returned as it is.
---@param guard_handle mare.host.GuardHandle
---@param method string
---@return mare.host.MethodGuard?
function Contracts.method_guard(guard_handle, method) end

---@class mare.host.MethodGuard
---@field passthrough boolean
---@field enter? fun(...): table? an error value, or nil to proceed
---@field leave? fun(packed: table): any, table? the value, or nil and an error value
---@field fail fun(err: any): table the error value a caller catches

--- The contract's `registration = true` methods (before `export`).
---@param handle mare.host.ContractHandle
---@return string[]
function Contracts.registrations(handle) end

--- Register what `handle` admitted: the JSON export, the catalog, `permitted` / `validate`. Once
--- per handle.
---@param handle mare.host.ContractHandle
---@param value any
---@return integer seq the registration
function Contracts.export(handle, value) end

--- Remove registration `seq` of `owner` at once (idempotent, synchronous: it runs in effect
--- cleanups). In-flight JSON calls end `unavailable`.
---@param owner integer
---@param seq integer
---@return boolean removed
function Contracts.withdraw(owner, seq) end

--- Whether the explicit caller (or unrestricted host call when omitted) may call `service.method`.
---@param service string
---@param method string
---@param caller? any
---@return boolean
function Contracts.permitted(service, method, caller) end

--- Whether the explicit caller is a restricted peer RPC connection (never true for owners or in-process Lua).
---@param caller? any
---@return boolean
function Contracts.is_peer(caller) end

--- Whether the explicit caller may address sessions by filesystem path (local/admin owner only).
---@param caller? any
---@return boolean
function Contracts.allows_session_path(caller) end

--- Check `value` against `service.method`'s result contract without calling anything.
---@param service string
---@param method string
---@param value any
---@return true
function Contracts.validate(service, method, value) end

--- Public (were `ctx:permitted` / `ctx:validate`): the same two checks, for any plugin. Installed by the
--- `contracts` host plugin (tree mode).
---@class mare.contracts
mare.contracts = {}

--- Whether the explicit caller (or unrestricted host call when omitted) may call `service.method`.
---@param service string
---@param method string
---@param caller? any
---@return boolean
function mare.contracts.permitted(service, method, caller) end

--- Whether the explicit caller is a restricted peer RPC connection (never true for owners or in-process Lua).
---@param caller? any
---@return boolean
function mare.contracts.is_peer(caller) end

--- Whether the explicit caller may address sessions by filesystem path (local/admin owner only).
---@param caller? any
---@return boolean
function mare.contracts.allows_session_path(caller) end

--- Check `value` against `service.method`'s result contract without calling anything.
---@param service string
---@param method string
---@param value any
---@return true ok raises `invalid_result: ...` otherwise
function mare.contracts.validate(service, method, value) end
