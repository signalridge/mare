---@meta _
-- The boot fragment's own capability entries in `config.prims`, the `task` host plugin's service,
-- and handle types returned by `kernel` host capability methods (`mare.host.KernelService` methods
-- are generated in `types/mare/services/kernel.gen.lua` from `contracts/kernel.lua`).
-- Public primitives for host plugins only: never a global, never requirable.

--- `config.prims`, the boot fragment's entries.
---@class mare.host.KernelPrims
local KernelPrims = {}

--- Report a plugin callback's error as the kernel does for its own callbacks: a problem line,
--- the log, and a `kernel/error` event (`what` names the callback, e.g. `"cleanup"`).
---@param plugin string
---@param what string
---@param err string
function KernelPrims.handler_failed(plugin, what, err) end

--- Drop plugin `name`'s modules from `package.loaded`; the next `require` reads their files
--- again. The plugin component (mare_kernel/component.lua) calls it when a plugin's fiber unloads.
---@param name string
function KernelPrims.unload_modules(name) end

--- Transitional: mark plugin `name` active with
--- its owner id in the kernel's active set, or inactive with `nil`.
---@param name string
---@param owner integer|nil
function KernelPrims.mark_active(name, owner) end

--- The `task` service (`mare_kernel.plugins.task`): tasks owned by the caller's fiber, aborted
--- when it unloads. Mixed into contexts as `ctx:after`, `ctx:every`, `ctx:spawn`; a plugin reads
--- them through its inject (`inject = { "task" }`).
---@class mare.host.Task
local Task = {}

--- Run `fn` once after `ms` milliseconds.
---@param ms number
---@param fn fun()
---@return mare.Task
function Task:after(ms, fn) end

--- Run `fn` every `ms` milliseconds until it returns false or the fiber unloads.
---@param ms number
---@param fn fun(): boolean|nil
---@return mare.Task
function Task:every(ms, fn) end

--- Start `fn(...)` as a task owned by the caller's fiber.
---@param fn fun(...)
---@return mare.Task
function Task:spawn(fn, ...) end

--- Exclusive per-host request/reload lease returned by `kernel:request_lease`.
--- Released by `:release()`, `<close>`, cancellation of the caller owner / request token / session,
--- or when the originating coroutine finishes.
---@class mare.host.RequestLease
local RequestLease = {}

--- Release the held lease immediately (idempotent).
function RequestLease:release() end

--- Atomic session identity switch handle returned by `kernel:begin_session_switch`.
--- Supports `<close>` and automatically aborts when the handle closes or its caller fiber ends.
--- Note: `abort()` only releases the target session reservation and switch lock; it does NOT roll
--- back any state changes already made by the provider.
---@class mare.host.SessionSwitchHandle
local SessionSwitchHandle = {}

--- Synchronously commit the session identity switch to `session_id` (a no-op when equal to the
--- current session id) and automatically mark any active session export call as committed.
--- Must not yield. Raises a coded error if already committed or aborted, or
--- `session.already_loaded` if a new target conflicts.
---@param session_id? string
---@return { session_epoch: string }
function SessionSwitchHandle:commit(session_id) end

--- Transfer ownership of this switch handle to the current coroutine (or detach from the
--- originating coroutine during a task handoff when `bind_current == false`).
---@param bind_current? boolean
function SessionSwitchHandle:adopt(bind_current) end

--- Abort the pending switch (idempotent). Only releases the reservation and lock; does NOT roll
--- back provider-side mutations.
function SessionSwitchHandle:abort() end
