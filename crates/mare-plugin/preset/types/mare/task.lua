---@meta _
-- LuaLS definitions: cancellation tokens, tasks, channels and clocks (entry: types/mare.lua)

-- ---------------------------------------------------------------------------
-- primitives
-- ---------------------------------------------------------------------------

---@class mare.Token
local Token = {}
function Token:cancel() end
---@return boolean
function Token:is_cancelled() end
---@return mare.Token
function Token:child() end
---@async
function Token:wait() end

---@class mare.Task
local Task = {}
---@async
---@return any ...
function Task:wait() end
function Task:abort() end
---@return boolean
function Task:done() end

--- Tasks, channels, sleeping and clocks. Timers (one-shot and periodic) are `ctx:after(ms, fn)` and
--- `ctx:every(ms, fn)`: they belong to the plugin, so unloading or reloading it cancels them; the
--- returned `mare.Task` cancels one early with `:abort()`, and an error in `fn` is shown in the UI
--- without stopping an `every` timer (return `false` to stop it).
---@class mare.task
mare.task = {}
---@param fn async fun(...): any
---@return mare.Task
function mare.task.spawn(fn, ...) end
---@async
---@param ms number
---@param cancel? mare.Token
---@return boolean elapsed false when cancelled
function mare.task.sleep(ms, cancel) end
---@class mare.Sender
local Sender = {}
---@param v any
---@return boolean
function Sender:send(v) end
---@return boolean
function Sender:is_closed() end

---@class mare.BoundedSender
local BoundedSender = {}
---@async
---@param v any
---@return boolean
function BoundedSender:send(v) end
---@param v any
---@return boolean
function BoundedSender:try_send(v) end
---@return boolean
function BoundedSender:is_closed() end

---@class mare.Receiver
local Receiver = {}
---@async
---@return any
function Receiver:recv() end
---@return any
function Receiver:try_recv() end
function Receiver:close() end

---@overload fun(): mare.Sender, mare.Receiver
---@param capacity integer positive capacity for a bounded channel
---@return mare.BoundedSender, mare.Receiver
function mare.task.channel(capacity) end
---@return mare.Token
function mare.task.token() end
--- Run functions concurrently; abort losing branches and wait for their Lua tasks to finish
--- before returning the first branch's index and results. Cancelling select requests abort of
--- every branch. A losing branch's cancellation or closing error does not replace the winner.
--- Synchronous code and closing hooks can delay cleanup; separately spawned tasks,
--- blocking work, processes and remote side effects may continue after the Lua task ends.
---@async
---@param ... async fun(): any
---@return integer, any ...
function mare.task.select(...) end
--- On timeout, closes the callback's Lua scope before returning false, including in a tool
--- context. Independently spawned work and blocking operations can still outlive the callback.
---@async
---@param ms number
---@param fn async fun(...): any
---@return boolean finished, any ...
function mare.task.timeout(ms, fn, ...) end
---@return number epoch ms
function mare.task.now() end

--- Synchronous call in the same execution context; yielding is an error.
---@param fn function
---@param ... any
---@return ... any
function mare.task.call_sync(fn, ...) end

--- Host-only synchronous publication. A thrown error or yield poisons readiness before
--- returning. Does not roll back callbacks/IO; prepare everything before publishing.
---@param fn function
---@param ... any
---@return ... any
function mare.task.publish(fn, ...) end
--- Host-only unrecoverable publication diagnostic; blocks future admission until rebuilt.
---@param reason string
function mare.task.fail_session(reason) end
--- Current unrecoverable session publication diagnostic, or nil when healthy.
---@return string|nil
function mare.task.session_error() end
--- Start a completion task owned by ctx. Registration failure poisons the Host synchronously.
--- Refused in tool contexts, including full-access/inherited tools.
---@param ctx mare.Ctx
---@param fn function
---@return mare.Task
function mare.task.spawn_completion(ctx, fn) end

---@return number monotonic ms
function mare.task.clock() end
--- Identifier of the running async task, or nil outside a Tokio task.
---@return string?
function mare.task.id() end
---@param value any
---@return boolean
function mare.task.is_token(value) end
---@async
function mare.task.yield() end
