---@meta cordis
-- `require("cordis")`: the plugin framework the plugin tree runs on (cordis-lua, a Lua port of
-- Cordis; vendored in crates/mare-kernel/lua/cordis). Public for plugins in the surface the native
-- plugin conventions use: a plugin's ctx (`cordis.Context`),
-- `cordis.class` to write a `mare.Service` subclass, and `cordis.Service`. The host's own entry
-- points (the root context, the host, the loader) are not part of it.

---@class cordis
---@field FiberState {PENDING: integer, LOADING: integer, ACTIVE: integer, FAILED: integer, DISPOSED: integer, UNLOADING: integer} Lifecycle states exported by Cordis.
local cordis = {}

--- A plugin's context: what `apply(ctx, config)` receives. Services are read as fields
--- (`ctx.tools`) once declared in the definition's `inject`.
---@class cordis.Context
---@field logger cordis.LoggerService the plugin's logger (`ctx.logger:warn(...)`, or `ctx.logger(name)`)
---@field policy mare.PolicyService the policy service (`inject = { "policy" }`)
---@field bus mare.BusService mare events (`inject = { "bus" }`)
---@field kernel mare.host.KernelService kernel introspection and reload (`inject = { "kernel" }`)
local Context = {}

--- An effect of this context's fiber: `execute` runs now and returns its cleanup, which runs when
--- the fiber unloads (or when the returned disposer is called; it is idempotent).
---@param execute fun(): (fun()|nil)
---@param label? string
---@return fun() dispose
function Context:effect(execute, label) end

--- Listen to `name` for this fiber's lifetime; a boolean is shorthand for `prepend`.
---@param name string
---@param listener function
---@param options? boolean|{ prepend?: boolean, global?: boolean }
---@return fun() dispose
function Context:on(name, listener, options) end

--- `on`, removed after the first call.
---@param name string
---@param listener function
---@param options? boolean|{ prepend?: boolean, global?: boolean }
---@return fun() dispose
function Context:once(name, listener, options) end

--- Call every listener of `name` (Cordis `emit`).
---@overload fun(self: cordis.Context, thisArg: table|function, name: string, ...: any)
---@param name string
---@param ... any
function Context:emit(name, ...) end

--- Call the listeners of `name` concurrently; settles when all did.
---@overload fun(self: cordis.Context, thisArg: table|function, name: string, ...: any): cordis.Future
---@param name string
---@param ... any
---@return cordis.Future future
function Context:parallel(name, ...) end

--- Call listeners in order until one returns a value other than nil, false, or cordis.null.
---@overload fun(self: cordis.Context, thisArg: table|function, name: string, ...: any): cordis.Future
---@param name string
---@param ... any
---@return cordis.Future future
function Context:serial(name, ...) end

--- The first value other than nil, false, or cordis.null a listener of `name` returns.
---@overload fun(self: cordis.Context, thisArg: table|function, name: string, ...: any): any
---@param name string
---@param ... any
---@return any
function Context:bail(name, ...) end

--- Call wrap-around listeners `fn(..., next)` with the event arguments, ending at the final
--- callback. `next()` invokes the next listener with the original arguments.
---@overload fun(self: cordis.Context, thisArg: table|function, name: string, ...: any): any
---@param name string
---@param ... any event arguments followed by the terminal callback
---@return any
function Context:waterfall(name, ...) end

--- A child fiber that runs `apply(sub)` while every named service is up, and is undone when one
--- goes or is replaced (the way to use an optional service).
---@param names string[]
---@param apply fun(sub: cordis.Context)
---@return cordis.Fiber fiber
function Context:inject(names, apply) end

---@overload fun(self: cordis.Context, name: "bus"): mare.BusService|nil
---@overload fun(self: cordis.Context, name: "kernel"): mare.host.KernelService|nil
---@overload fun(self: cordis.Context, name: "policy"): mare.PolicyService|nil
--- The service `name` as this context reads it (traced: a method's `self.ctx` is this context),
--- or nil when it is not up. For an optional service not declared in `inject`.
---@param name string
---@return mare.InjectedService|nil
function Context:get(name) end

--- A child fiber returned by `inject`.
---@class cordis.Fiber
---@field state integer Current lifecycle state, one of `cordis.FiberState`.
---@field dispose fun(self: cordis.Fiber): cordis.Future
---@field await fun(self: cordis.Fiber): cordis.Future

---@class cordis.Future
---@field await fun(self: cordis.Future): any

--- Use a component (a `cordis.class` such as a `mare.Service` subclass, or a definition table) as
--- a child of this context; returns its fiber.
---@param component any
---@param config? any
---@return any fiber
function Context:plugin(component, config) end

---@class cordis.Logger
---@field debug fun(self, ...)
---@field info fun(self, ...)
---@field warn fun(self, ...)
---@field error fun(self, ...)

--- The logger service: a logger named after the plugin (its methods), or one named `name` when
--- called; exported to mare's log.
---@class cordis.LoggerService : cordis.Logger
---@overload fun(name?: string): cordis.Logger

--- A service provided by a fiber: the base class of `mare.Service`.
---@class cordis.Service
---@field ctx mare.Ctx the context the current call comes from (the caller, in a method)
cordis.Service = {}

--- Create a class `name` deriving from `parent`; instances are made with `Class.new(...)` or used
--- as components with `ctx:plugin(Class)`. `Class.super` is the parent class.
---@param name string
---@param parent? table
---@return table
function cordis.class(name, parent) end

return cordis
