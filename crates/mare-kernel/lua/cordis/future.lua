-- Future: the Lua counterpart of a JS Promise.
--
-- A Future is bound to a `cordis.Host`. Reactions (`next` / `catch` / `finally`)
-- never run synchronously inside `resolve` / `reject`: they are queued on a
-- per-host job queue that is drained by a host task, like JS microtasks.
-- `await` blocks the current host task; it must be called inside a task.
local M = {}

local PENDING, FULFILLED, REJECTED = 0, 1, 2

---@class cordis.Future
local Future = {}
Future.__index = Future
Future.__name = "cordis.Future"
M.Future = Future

local function isFuture(x)
  return type(x) == "table" and getmetatable(x) == Future
end
Future.is = isFuture
M.isFuture = isFuture

-- per-host job queue ---------------------------------------------------------

local queues = setmetatable({}, { __mode = "k" })

-- the host a context or a Future exposes is a
-- facade whose `spawn` starts work in the caller's own context, never in the
-- host's root context; only this module reaches the real host (the job queue's
-- own tasks), through this private map
local realHosts = setmetatable({}, { __mode = "k" })

local function root(host)
  return realHosts[host] or host
end

---The facade of `host` for contexts and Futures (idempotent). Without
---`capture` its `spawn` is the host's; with it, `spawn` runs in the caller's
---captured context.
function M.facade(host)
  if realHosts[host] then
    return host
  end
  assert(type(host) == "table" and host.spawn and host.latch, "Future requires a cordis.Host")
  local capture = host.capture
  local facade = {
    latch = host.latch,
    sleep = host.sleep,
    now = host.now,
    capture = capture,
    spawn = capture and function(fn)
      capture().spawn(fn)
    end or host.spawn,
  }
  realHosts[facade] = host
  return facade
end

-- Tasks spawned by the library remember their host (see `M.spawn`).
local taskHosts = setmetatable({}, { __mode = "k" })

-- Run `fn` after the current synchronous segment, even on a host whose `spawn`
-- starts tasks eagerly: the task first waits on a latch that is opened only
-- after `spawn` returns, and opening a latch never resumes waiters inline.
local function defer(host, fn)
  local latch = host.latch()
  root(host).spawn(function()
    taskHosts[coroutine.running()] = host
    latch:wait()
    fn()
  end)
  latch:open()
end
M.defer = defer

-- the queue each running drainer task is draining
local drainers = setmetatable({}, { __mode = "k" })

local function startDrainer(host, q)
  q.scheduled = true
  defer(host, function()
    local me = coroutine.running()
    q.drainer = me
    drainers[me] = q
    local h = q.head
    while h <= q.tail do
      local j, argc, a, b = q[h], q[h + 1], q[h + 2], q[h + 3]
      q[h], q[h + 1], q[h + 2], q[h + 3] = nil, nil, nil, nil
      h = h + 4
      q.head = h
      local ok, err
      if argc == 2 then
        ok, err = pcall(j, a, b)
      else
        ok, err = pcall(j)
      end
      if not ok then
        -- report like an uncaught error of its own task; the queue goes on
        root(host).spawn(function()
          error(err, 0)
        end)
      end
      -- the job blocked and handed the queue over to a new drainer
      if q.drainer ~= me then
        return
      end
    end
    q.head, q.tail = 1, 0
    drainers[me] = nil
    q.drainer = nil
    q.scheduled = false
  end)
end

local function enqueueJob(host, job, argc, a, b)
  local key = root(host)
  local q = queues[key]
  if not q then
    q = { head = 1, tail = 0, scheduled = false, host = key }
    queues[key] = q
  end
  local t = q.tail
  q[t + 1] = job
  q[t + 2] = argc or 0
  q[t + 3] = a
  q[t + 4] = b
  q.tail = t + 4
  if not q.scheduled then
    startDrainer(key, q)
  end
end

-- Called before the current task blocks: if it is draining a job queue (of
-- whichever host), hand that queue over to a fresh drainer so that its pending
-- jobs are not starved.
local function release()
  local me = coroutine.running()
  local q = drainers[me]
  if q and q.drainer == me then
    drainers[me] = nil
    q.drainer = nil
    startDrainer(q.host, q)
  end
end
M.release = release
function M.queueMicrotask(host, job)
  enqueueJob(host, job, 0)
end

-- execution contexts -------------------------------------------

---The execution context of the running code, if the host has `capture`:
---`host.capture() -> scope`, where `scope.run(fn, ...)` calls
---`fn` right away in the captured context and returns its results (the
---caller waits), and `scope.spawn(fn)` starts `fn` as a task in that context.
function M.capture(host)
  local capture = host.capture
  if capture then
    return capture()
  end
end

---Is the running task one of the library's job-queue drainers?
function M.inDrainer()
  return drainers[coroutine.running()] ~= nil
end

---Call a user callback at a point upstream calls it synchronously (listeners
---of emit / bail / waterfall): in the context it was registered in.
function M.runSync(host, scope, fn, ...)
  if scope == nil then
    return M.call(host, fn, ...)
  end
  return scope.run(M.call, host, fn, ...)
end

---Run a user callback that was handed to the library when `scope` was
---captured and is invoked later (a component, a disposer, a reaction). Without
---a scope (a host without `capture`) it runs inline, as before. Outside a
---drainer it runs through `scope.run`, keeping upstream's synchronous result.
---Inside a drainer it runs as its own task in the captured context and a Future
---of its result is returned: a drainer only runs library bookkeeping, so a
---callback that blocks natively holds up nothing but itself.
function M.run(host, scope, fn, ...)
  if scope == nil then
    return M.call(host, fn, ...)
  end
  if not drainers[coroutine.running()] then
    return M.runSync(host, scope, fn, ...)
  end
  return M.start(host, scope, fn, ...)
end

-- the rejection reason of a cancelled task's Future
M.CANCELLED = setmetatable({}, {
  __tostring = function()
    return "cordis: task cancelled"
  end,
})

---Start a user callback as its own task in `scope` and return a Future of its
---result (with `scope` nil: call it inline, as `M.call`). When the
---scope's `spawn` returns a handle with `cancel`, a second value is returned, a
---function that cancels the task if it has started and not finished, and then
---rejects the Future with `M.CANCELLED`.
function M.start(host, scope, fn, ...)
  if scope == nil then
    return M.call(host, fn, ...)
  end
  local args = table.pack(...)
  local f, resolve, reject = Future.withResolvers(host)
  local started, done = false, false
  local taskCo
  local handle = scope.spawn(function()
    started = true
    taskCo = coroutine.running()
    taskHosts[taskCo] = host
    local ok, result = pcall(M.call, host, fn, table.unpack(args, 1, args.n))
    done = true
    if ok then
      resolve(result)
    else
      reject(result)
    end
  end)
  if type(handle) == "table" and type(handle.cancel) == "function" then
    -- only a task that started and is suspended part-way is cancelled; one that has not
    -- started yet runs as upstream would (returns whether it cancelled)
    return f,
      function()
        if not started or done or coroutine.running() == taskCo then
          return false
        end
        if handle.cancel() == false then
          return false
        end
        reject(M.CANCELLED)
        return true
      end
  end
  return f
end

-- functions the library itself hands to its effects (bookkeeping disposers):
-- they are not user callbacks and keep running inline
local internals = setmetatable({}, { __mode = "k" })

---Mark `fn` as library-internal (see `M.run`); returns `fn`.
function M.internal(fn)
  internals[fn] = true
  return fn
end

function M.isInternal(fn)
  return internals[fn] == true
end

---A Future settled one hop later. Without `capture` that is one job hop; with
---it, one host task turn, because user callbacks invoked from the job queue
---then start as tasks: a continuation that must come after such callbacks
---started (upstream: after their microtask) waits for a task turn too.
function M.hop(host)
  if not host.capture then
    return Future.resolve(host, nil)
  end
  local f, resolve = Future.withResolvers(host)
  M.spawn(host, function()
    resolve(nil)
  end)
  return f
end

---Spawn `fn` as a task in `scope` (or as a plain library task without one).
function M.spawnIn(host, scope, fn)
  if scope == nil then
    return M.spawn(host, fn)
  end
  scope.spawn(function()
    taskHosts[coroutine.running()] = host
    fn()
  end)
end

-- construction ----------------------------------------------------------------

local function settle(self, state, value)
  if self._state ~= PENDING then
    return
  end
  self._state = state
  self._value = value
  local reactions = self._reactions
  if reactions then
    self._reactions = nil
    for i = 1, #reactions do
      enqueueJob(self._host, reactions[i], 2, state, value)
    end
  end
end

local function subscribe(self, reaction)
  if self._state == PENDING then
    local reactions = self._reactions
    if reactions then
      reactions[#reactions + 1] = reaction
    else
      self._reactions = { reaction }
    end
  else
    enqueueJob(self._host, reaction, 2, self._state, self._value)
  end
end

local function resolveWith(self, value)
  if self._state ~= PENDING or self._locked then
    return
  end
  if value == self then
    return settle(self, REJECTED, "cannot resolve a future with itself")
  end
  if isFuture(value) then
    self._locked = true
    self._handled = true
    value._handled = true
    subscribe(value, function(state, v)
      settle(self, state, v)
    end)
    return
  end
  -- thenables (fibers, effect disposers): `__future(x)` gives the Future to
  -- adopt; its value is taken as is, so a fiber resolving to itself is fine
  local toFuture = type(value) == "table" and getmetatable(value)
  toFuture = type(toFuture) == "table" and toFuture.__future
  if toFuture then
    self._locked = true
    local ok, fut = pcall(toFuture, value)
    if not ok then
      return settle(self, REJECTED, fut)
    end
    fut._handled = true
    subscribe(fut, function(state, v)
      settle(self, state, v)
    end)
    return
  end
  settle(self, FULFILLED, value)
end

---Is `x` a Future or a thenable (`__future` metamethod)?
local function isThenable(x)
  if isFuture(x) then
    return true
  end
  local mt = type(x) == "table" and getmetatable(x)
  return type(mt) == "table" and mt.__future ~= nil
end
M.isThenable = isThenable

local function rejectWith(self, reason)
  if self._state ~= PENDING or self._locked then
    return
  end
  settle(self, REJECTED, reason)
end

local function create(host)
  if not realHosts[host] then
    assert(type(host) == "table" and host.spawn and host.latch, "Future requires a cordis.Host")
  end
  return setmetatable({ _host = host, _state = PENDING }, Future)
end

---Create a pending future and its resolvers (`Promise.withResolvers`).
function Future.withResolvers(host)
  local f = create(host)
  return f, function(v)
    resolveWith(f, v)
  end, function(r)
    rejectWith(f, r)
  end
end

---`new Promise(executor)`: the executor runs synchronously; an error rejects.
function Future.new(host, executor)
  local f, resolve, reject = Future.withResolvers(host)
  local ok, err = pcall(executor, resolve, reject)
  if not ok then
    reject(err)
  end
  return f
end

function Future.resolve(host, value)
  if type(value) ~= "table" then
    if not realHosts[host] then
      assert(type(host) == "table" and host.spawn and host.latch, "Future requires a cordis.Host")
    end
    return setmetatable({ _host = host, _state = FULFILLED, _value = value }, Future)
  end
  if getmetatable(value) == Future then
    return value
  end
  local f = create(host)
  resolveWith(f, value)
  return f
end

---Fulfill `f` with `value` as is (no adoption of thenables); for thenables
---whose own Future resolves to themselves.
function M.fulfill(f, value)
  settle(f, FULFILLED, value)
end

function Future.reject(host, reason)
  local f = create(host)
  settle(f, REJECTED, reason)
  return f
end

-- chaining --------------------------------------------------------------------

---`then` is a Lua keyword, so it is spelled `next`.
-- `next` without capturing: for the library's own bookkeeping reactions
function Future:_next(onFulfilled, onRejected)
  local f = create(self._host)
  self._handled = true
  subscribe(self, function(state, value)
    local handler
    if state == FULFILLED then
      handler = onFulfilled
    else
      handler = onRejected
    end
    if handler == nil then
      if state == FULFILLED then
        settle(f, FULFILLED, value)
      else
        settle(f, REJECTED, value)
      end
      return
    end
    local ok, result = pcall(handler, value)
    if ok then
      resolveWith(f, result)
    else
      rejectWith(f, result)
    end
  end)
  return f
end

---`then` is a Lua keyword, so it is spelled `next`. On a host with `capture`
---the handlers run in the execution context captured here.
function Future:next(onFulfilled, onRejected)
  local host = self._host
  local scope = M.capture(host)
  if scope then
    local f, g = onFulfilled, onRejected
    onFulfilled = f and function(value)
      return M.run(host, scope, f, value)
    end
    onRejected = g and function(reason)
      return M.run(host, scope, g, reason)
    end
  end
  return self:_next(onFulfilled, onRejected)
end

function Future:catch(onRejected)
  return self:next(nil, onRejected)
end

function Future:_catch(onRejected)
  return self:_next(nil, onRejected)
end

function Future:finally(onFinally)
  if onFinally == nil then
    return self:next(nil, nil)
  end
  local host = self._host
  local out = create(host)
  self
    :next(function(value)
      return Future.resolve(host, onFinally()):_next(function()
        settle(out, FULFILLED, value)
      end, function(err)
        settle(out, REJECTED, err)
      end)
    end, function(reason)
      return Future.resolve(host, onFinally()):_next(function()
        settle(out, REJECTED, reason)
      end, function(err)
        settle(out, REJECTED, err)
      end)
    end)
    :_catch(function(err)
      settle(out, REJECTED, err)
    end)
  return out
end

-- inspection ------------------------------------------------------------------

function Future:state()
  return self._state == PENDING and "pending" or self._state == FULFILLED and "fulfilled" or "rejected"
end

function Future:host()
  return self._host
end

-- awaiting --------------------------------------------------------------------

---Block the current host task until the future settles. Returns the value, or
---raises the rejection reason. A settled future returns immediately.
function Future:await()
  self._handled = true
  if self._state == PENDING then
    -- a latch the waiting task can wait on: its own host's, when known
    local latch = (taskHosts[coroutine.running()] or self._host).latch()
    subscribe(self, function()
      latch:open()
    end)
    release()
    latch:wait()
  end
  if self._state == REJECTED then
    error(self._value, 0)
  end
  return self._value
end

---Yield the current task until the job queue has run once (the counterpart of
---`await Promise.resolve()`). Works on eager and deferred hosts alike.
function M.tick(host)
  local latch = host.latch()
  enqueueJob(host, function()
    latch:open()
  end)
  release()
  latch:wait()
end

-- combinators -----------------------------------------------------------------

local function toFuture(host, x)
  if isFuture(x) then
    return x
  end
  return Future.resolve(host, x)
end

function Future.all(host, list)
  local n = #list
  local f, resolve, reject = Future.withResolvers(host)
  local results, remaining = {}, n
  if n == 0 then
    resolve(results)
    return f
  end
  for i = 1, n do
    toFuture(host, list[i]):_next(function(v)
      results[i] = v
      remaining = remaining - 1
      if remaining == 0 then
        resolve(results)
      end
    end, reject)
  end
  return f
end

function Future.allSettled(host, list)
  local n = #list
  local f, resolve = Future.withResolvers(host)
  local results, remaining = {}, n
  if n == 0 then
    resolve(results)
    return f
  end
  local function done(i, r)
    results[i] = r
    remaining = remaining - 1
    if remaining == 0 then
      resolve(results)
    end
  end
  for i = 1, n do
    toFuture(host, list[i]):_next(function(v)
      done(i, { status = "fulfilled", value = v })
    end, function(e)
      done(i, { status = "rejected", reason = e })
    end)
  end
  return f
end

-- ambient host ----------------------------------------------------------------

---Spawn `fn` as a host task that knows its host. On a host with `capture` it
---starts in the caller's context through the public host facade, never in
---the host's root context.
function M.spawn(host, fn)
  local real = root(host)
  local spawn = real.capture and function(body)
    real.capture().spawn(body)
  end or real.spawn
  spawn(function()
    taskHosts[coroutine.running()] = host
    fn()
  end)
end

---Associate the running task with `host` (for tasks not spawned by the library).
function M.enter(host)
  taskHosts[coroutine.running()] = host
end

function M.currentHost()
  return taskHosts[coroutine.running()]
end

---@class cordis.AsyncFunction
local AsyncFunction = {}
AsyncFunction.__name = "cordis.AsyncFunction"
M.AsyncFunction = AsyncFunction

function AsyncFunction.is(x)
  return getmetatable(x) == AsyncFunction
end

local function findHost(args)
  for i = 1, args.n do
    local a = args[i]
    if type(a) == "table" then
      local mt = getmetatable(a)
      if mt == Future then
        return a._host
      end
      local finder = type(mt) == "table" and rawget(mt, "__cordis_host")
      if finder then
        return finder(a)
      end
      -- a Context (root, derived, isolated or traced) carries its host
      local Context = package.loaded["cordis.context"]
      if Context and Context.is(a) then
        local host = a.host
        if host then
          return host
        end
      end
    end
  end
end

---Run the async function on `host`; returns a Future of its result.
function AsyncFunction:spawn(host, ...)
  local args = table.pack(...)
  local fn = self.fn
  local f, resolve, reject = Future.withResolvers(host)
  -- the body runs in the execution context of the call
  local scope = M.capture(host)
  local spawn = scope
      and function(body)
        scope.spawn(function()
          taskHosts[coroutine.running()] = host
          body()
        end)
      end
    or function(body)
      M.spawn(host, body)
    end
  spawn(function()
    local ok, result = pcall(fn, table.unpack(args, 1, args.n))
    if ok then
      resolve(result)
    else
      reject(result)
    end
  end)
  return f
end

function AsyncFunction:__call(...)
  local args = table.pack(...)
  local host = M.currentHost() or findHost(args)
  if not host then
    error("cordis.async: no host available; call it from a cordis task or pass a Context / Future argument", 2)
  end
  return AsyncFunction.spawn(self, host, ...)
end

---`async function`: returns a callable that, on every call, runs `fn` in a new
---host task and immediately returns a Future of its result. The host is the
---one of the calling task, or found from a Context / Future argument.
function M.async(fn)
  return setmetatable({ fn = fn }, AsyncFunction)
end

---Call `fn` (a plain function, callable table or AsyncFunction) on behalf of
---the library: an AsyncFunction is spawned on `host` directly.
function M.call(host, fn, ...)
  if getmetatable(fn) == AsyncFunction then
    return AsyncFunction.spawn(fn, host, ...)
  end
  return fn(...)
end

---`await x` where `x` may or may not be a Future.
function M.await(x)
  if isFuture(x) then
    return x:await()
  end
  if type(x) == "table" then
    local mt = getmetatable(x)
    local aw = type(mt) == "table" and mt.__await
    if not aw and type(mt) == "table" and mt.__future then
      return mt.__future(x):await()
    end
    if aw then
      return aw(x)
    end
  end
  return x
end

return M
