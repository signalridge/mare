-- Ported from cordis packages/timer/src/index.ts (MIT, (c) Shigma)
--
-- Deviations (docs/deviations.md, timer section): timers are built on
-- `host.spawn` + `host.sleep`, throttling reads `host.now`; `timeout(delay)`
-- returns a Future; `interval(delay)` returns a table with `next` /
-- `return` / `throw`; a first argument that is not a number is the
-- callback; scheduled wrappers are callable tables with `dispose`.
local cordis = require("cordis")
local Future = cordis.Future

local unpack = table.unpack
local future = require("cordis.future")

-- a timer task runs in `scope`, the execution context its callback was handed in
-- (captured when the timer was scheduled, or when throttle / debounce was created)
local function spawn(host, scope, fn)
  future.spawnIn(host, scope, fn)
end

-- Node's timers run a delay below 1 ms (0, negative) at 1 ms
local function delayOf(ms)
  if type(ms) ~= "number" or ms ~= ms or ms < 1 then
    return 1
  end
  return ms
end

-- A timer callback that raises is an uncaught exception upstream; here it is
-- reported through the logger of the context that scheduled it.
local function invoke(ctx, fn, ...)
  local ok, err = pcall(fn, ...)
  if not ok then
    ctx.logger("timer"):error(err)
  end
end

local TimerService = cordis.class("TimerService", cordis.Service)

-- Sleep until `deadline` (host.now() time). The deadline is taken when the
-- timer is registered, as with `setTimeout`, since a host may start the task
-- that waits for it later.
-- A deadline already reached still yields one turn of the host (`sleep(0)`), so a timer whose
-- callback overruns its period cannot keep the task from yielding. The handle's
-- latch ends the sleep early when the timer is cleared, on a host whose sleep takes it.
local function sleepUntil(host, deadline, latch)
  local rest = deadline - host.now()
  host.sleep(rest > 0 and rest or 0, latch)
end

-- a timer handle: `clear` sets the flag and opens the latch
local function handleOf(host)
  return { cancelled = false, latch = host.latch() }
end

-- `setTimeout(fn, ms, ...args)` on the host in `scope`; returns the handle for `clear`
local function setTimeoutIn(ctx, scope, fn, ms, ...)
  local host = ctx.host
  local args = table.pack(...)
  local handle = handleOf(host)
  local deadline = host.now() + delayOf(ms)
  spawn(host, scope, function()
    sleepUntil(host, deadline, handle.latch)
    if handle.cancelled then
      return
    end
    handle.cancelled = true
    invoke(ctx, fn, unpack(args, 1, args.n))
  end)
  return handle
end

-- `setTimeout(fn, ms, ...args)`: in the context of the caller
local function setTimeout(ctx, fn, ms, ...)
  return setTimeoutIn(ctx, future.capture(ctx.host), fn, ms, ...)
end

-- `setInterval(fn, ms)` on the host
local function setInterval(ctx, fn, ms)
  local host = ctx.host
  local handle = handleOf(host)
  local period = delayOf(ms)
  local deadline = host.now()
  spawn(host, future.capture(host), function()
    while true do
      -- ticks missed while the task was not running are skipped, not replayed
      deadline = math.max(deadline + period, host.now())
      sleepUntil(host, deadline, handle.latch)
      if handle.cancelled then
        return
      end
      invoke(ctx, fn)
    end
  end)
  return handle
end

-- `clearTimeout` / `clearInterval`
local function clear(handle)
  if handle then
    handle.cancelled = true
    handle.latch:open()
  end
end

local function disposedError()
  return cordis.Error.new("Context has been disposed")
end

function TimerService:constructor(ctx)
  TimerService.super.constructor(self, ctx, "timer")
  local host = ctx.host
  if type(host.sleep) ~= "function" or type(host.now) ~= "function" then
    error(cordis.Error.new("timer requires host.sleep and host.now"), 2)
  end
  ctx:mixin("timer", { "timeout", "interval", "throttle", "debounce", "setTimeout", "setInterval" })
end

--- @deprecated use `ctx:timeout()` instead
function TimerService:setTimeout(callback, delay)
  return self:timeout(callback, delay)
end

--- @deprecated use `ctx:interval()` instead
function TimerService:setInterval(callback, delay)
  return self:interval(callback, delay)
end

-- timeout(callback, delay) -> dispose
-- timeout(delay) -> Future
function TimerService:timeout(...)
  local callback, delay = ...
  if type(callback) == "number" then
    callback, delay = nil, callback
  end
  local host = self.ctx.host
  if callback then
    local dispose
    dispose = self.ctx:effect(function()
      local timer = setTimeout(self.ctx, function()
        dispose()
        callback()
      end, delay)
      return function()
        clear(timer)
      end
    end, "ctx.timeout()")
    return dispose
  else
    local promise, resolve, reject = Future.withResolvers(host)
    local dispose = self.ctx:effect(function()
      local timer = setTimeout(self.ctx, resolve, delay)
      return function()
        clear(timer)
        reject(disposedError())
      end
    end, "ctx.timeout()")
    return promise:finally(function()
      return dispose()
    end)
  end
end

-- interval(callback, delay) -> dispose
-- interval(delay) -> async iterator { next, return, throw }
function TimerService:interval(...)
  local callback, delay = ...
  if type(callback) == "number" then
    callback, delay = nil, callback
  end
  local host = self.ctx.host
  if callback then
    return self.ctx:effect(function()
      local timer = setInterval(self.ctx, function()
        callback()
      end, delay)
      return function()
        clear(timer)
      end
    end, "ctx.interval()")
  end

  local done -- { kind = "return", value } | { kind = "throw", reason }
  local nextTasks, head, tail = {}, 1, 0 -- { resolve, reject }
  local function popTask()
    if head > tail then
      return nil
    end
    local task = nextTasks[head]
    nextTasks[head] = nil
    head = head + 1
    if head > tail then
      head, tail = 1, 0
    end
    return task
  end
  local function drain(fn)
    local h, t = head, tail
    if h > t then
      return
    end
    local tasks = nextTasks
    nextTasks, head, tail = {}, 1, 0
    for i = h, t do
      fn(tasks[i])
    end
  end
  local dispose = self.ctx:effect(function()
    local timer = setInterval(self.ctx, function()
      local task = popTask()
      if task then
        task.resolve({ done = false, value = nil })
      end
    end, delay)
    return function()
      clear(timer)
      if done then
        return
      end
      done = { kind = "throw", reason = disposedError() }
      drain(function(task)
        task.reject(done.reason)
      end)
    end
  end, "ctx.interval()")

  local iterator = {}
  function iterator.next(_)
    if not done then
      local promise, resolve, reject = Future.withResolvers(host)
      tail = tail + 1
      nextTasks[tail] = { resolve = resolve, reject = reject }
      return promise
    end
    if done.kind == "return" then
      return Future.resolve(host, { done = true, value = done.value })
    end
    return Future.reject(host, done.reason)
  end
  iterator["return"] = function(_, value)
    if not done then
      done = { kind = "return", value = value }
    end
    drain(function(task)
      task.resolve({ done = true, value = value })
    end)
    dispose()
    return Future.resolve(host, { done = true, value = value })
  end
  function iterator.throw(_, reason)
    if not done then
      done = { kind = "throw", reason = reason }
    end
    drain(function(task)
      task.reject(reason)
    end)
    dispose()
    return Future.resolve(host, { done = true, value = nil })
  end
  return iterator
end

local Scheduled = { __name = "Scheduled" }
Scheduled.__index = Scheduled
function Scheduled:__call(...)
  return self.call(...)
end

function TimerService:_schedule(label, trigger)
  local timer
  local isDisposed = false
  local dispose = self.ctx:effect(function()
    return function()
      isDisposed = true
      clear(timer)
      timer = nil
    end
  end, label)
  return setmetatable({
    dispose = dispose,
    call = function(...)
      if isDisposed then
        return
      end
      clear(timer)
      timer = nil
      local nextTimer = trigger(table.pack(...))
      -- A re-entering callback may schedule a trailing call before `trigger` returns; keep its handle.
      if nextTimer ~= nil then
        timer = nextTimer
      end
    end,
  }, Scheduled)
end

-- the callback runs in the context it was handed in, captured here; the
-- leading call runs in it synchronously, trailing calls start as tasks in it
function TimerService:throttle(callback, delay, noTrailing)
  local host = self.ctx.host
  local scope = future.capture(host)
  local lastCall = -math.huge
  local function execute(...)
    lastCall = host.now()
    future.runSync(host, scope, callback, ...)
  end
  return self:_schedule("ctx.throttle()", function(args)
    local now = host.now()
    local remaining = delay - now + lastCall
    if remaining <= 0 then
      execute(unpack(args, 1, args.n))
    elseif not noTrailing then
      return setTimeoutIn(self.ctx, nil, execute, remaining, unpack(args, 1, args.n))
    end
  end)
end

function TimerService:debounce(callback, delay)
  local host = self.ctx.host
  local scope = future.capture(host)
  return self:_schedule("ctx.debounce()", function(args)
    return setTimeoutIn(self.ctx, scope, callback, delay, unpack(args, 1, args.n))
  end)
end

return TimerService
