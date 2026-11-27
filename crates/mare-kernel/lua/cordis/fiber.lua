-- Ported from cordis packages/core/src/fiber.ts (MIT, (c) Shigma)
--
-- Async methods follow one rule: the synchronous prefix of the upstream
-- `async` function runs synchronously, the rest continues in host tasks or
-- Future reactions, and the method returns a Future.
local proto = require("cordis.proto")
local class = require("cordis.class").class
local construct = require("cordis.class").construct
local symbols = require("cordis.symbols")
local S = symbols.symbols
local errors = require("cordis.error")
local future = require("cordis.future")
local Future = future.Future
local trace = require("cordis.trace")
local Set = require("cordis.ordered").Set
local DisposableList = require("cordis.list")
local injectIndex = require("cordis.inject_index")
local resolveEpoch = require("cordis.resolve_epoch")

local M = {}

local FiberState = {
  PENDING = 0,
  LOADING = 1,
  ACTIVE = 2,
  FAILED = 3,
  DISPOSED = 4,
  UNLOADING = 5,
}
M.FiberState = FiberState
local PENDING, LOADING, ACTIVE, FAILED, DISPOSED, UNLOADING = 0, 1, 2, 3, 4, 5

local INACTIVE = "__INACTIVE__"

-- effect iterators  ---------------------------------------------

local Iter = { __name = "cordis.Iter" }
Iter.__index = Iter
local AsyncIter = { __name = "cordis.AsyncIter" }
AsyncIter.__index = AsyncIter

---`function*`: `cordis.iter(function(yield, ...) yield(dispose) end)`.
---Returns a function that, when called with `...`, produces the iterable.
function M.iter(fn)
  return function(...)
    return setmetatable({ fn = fn, args = table.pack(...) }, Iter)
  end
end

---`async function*`: runs in a host task; `yield(dispose)` hands over a
---disposer and stops the body once the effect is disposed.
function M.asyncIter(fn)
  return function(...)
    return setmetatable({ fn = fn, args = table.pack(...) }, AsyncIter)
  end
end

-- the private signal that unwinds an aborted async iterator body
local ABORT = setmetatable({}, {
  __name = "cordis.abort",
  __tostring = function()
    return "effect aborted"
  end,
})
M.ABORT = ABORT

local function invalidEffect()
  error(errors.TypeError.new("Invalid effect"), 0)
end

-- the Fiber class -----------------------------------------------------------------

local Fiber = class("Fiber")
M.Fiber = Fiber

-- a user disposer runs in the execution context it was collected
-- in. Each registration gets its own entry (a library-internal closure over the
-- disposer and that scope), so one function collected from two contexts keeps
-- both. Library disposers (effect wrappers, internal closures) are kept as is.
local function remember(host, scope, d)
  if scope == nil or getmetatable(d) == M.Disposer or future.isInternal(d) then
    return d
  end
  return future.internal(function()
    return future.run(host, scope, d)
  end)
end

local function callDisposer(host, d)
  return future.call(host, d)
end

-- an effect disposer: callable, awaitable, carries `[Context.effect]` meta
local Disposer = { __name = "cordis.EffectDisposer" }
Disposer.__index = Disposer
function Disposer:__call()
  return self._dispose()
end
function Disposer:await()
  return self._future():await()
end
Disposer.__await = Disposer.await
-- `await dispose` / adoption by a Future: settles once setup finished, with
-- a plain disposer function
function Disposer.__future(self)
  return self._future()
end
M.Disposer = Disposer

local function emptyStack()
  return {}
end

function Fiber:constructor(parent, config, inject, runtime, options)
  options = options or {}
  local getOuterStack = options.getOuterStack or emptyStack
  self.parent = parent
  self.config = config
  self.inject = inject
  self.runtime = runtime
  self._options = options
  self.state = PENDING
  self.provisions = Set.new()
  self._hooks = {}
  self._disposables = DisposableList.new()
  self._revision = 0
  self._store = proto.create(nil)
  local this = self

  local function collect(dispose)
    this._disposables:push(remember(this.ctx.host, this._scope, dispose))
  end

  if runtime then
    self.uid = parent.registry.counter
    self.ctx = parent:extend({ fiber = self })
    self.context = self.ctx
    -- the component runs in the context of `use()`
    self._scope = future.capture(parent.host)

    self._runner = {
      epoch = INACTIVE,
      getOuterStack = getOuterStack,
      execute = function(fiber)
        return fiber:_executeComponent()
      end,
      scope = self._scope,
      collect = collect,
    }

    -- the fiber gets its parent-owned disposer before it is published
    self.dispose = parent.fiber:effect(function()
      this._remove = runtime.fibers:push(this)
      return future.internal(function()
        this.uid = nil
        local host = this.ctx.host
        -- observers on the teardown path are isolated from one another
        this.context.events:_emitIsolated(function(err)
          this.ctx.logger:error(err)
        end, "internal/plugin", this)
        if this._options.dispose then
          local okDispose, errDispose = pcall(this._options.dispose, this)
          if not okDispose then
            pcall(function()
              this.ctx.logger:error(errDispose)
            end)
          end
        end
        -- a fiber disposed while its component is still applying cancels that
        -- task, where the host can cancel it (otherwise the unload waits for it, as upstream)
        local cancel = this._cancelApply
        if cancel then
          this._cancelApply = nil
          cancel()
        end
        local rt = this.runtime
        this._remove()
        injectIndex.remove(this)
        if rt.fibers:length() == 0 then
          this.ctx.registry:_unregister(rt)
        end
        local okEpoch, errEpoch = pcall(this._setEpoch, this, INACTIVE)
        -- a PENDING fiber can already own effects (an
        -- internal/plugin observer, `fiber.ctx:effect`). Its epoch was already
        -- INACTIVE, so `_setEpoch()` drove no transition: unload that
        -- pre-activation work explicitly before reporting disposal complete.
        -- (only then: without effects the transitions stay upstream's)
        if not this.inertia and this._disposables:length() > 0 then
          this:_updateState(function()
            this.inertia = this:_unload()
            return UNLOADING
          end)
        end
        -- `inertia` never rejects: `_reload` and `_unload` both log their
        -- own work errors. A rejection here would mean the logger itself
        -- failed, so let it propagate.
        local function drain()
          if this.inertia then
            return this.inertia:_next(drain)
          end
          -- `_setEpoch()` above returns early for a fiber that was not ACTIVE,
          -- so the cached state is not refreshed. Re-derive it now that
          -- `uid` is nil; the empty callback falls through to `_getState()`.
          this:_updateState(function() end)
        end
        local settled = Future.resolve(host, nil):_next(drain)
        if not okEpoch then
          return settled:_next(function()
            error(errEpoch, 0)
          end)
        end
        return settled
      end)
    end, "ctx.use()")

    -- index after runtime.fibers registration so runtime buckets and dispose stay aligned
    injectIndex.set(self, inject)

    local ok, err = pcall(function()
      this.context:emit("internal/plugin", this)
      for _, name in ipairs(proto.ownKeys(this.inject)) do
        this:_checkImpl(name)
      end
    end)
    if not ok then
      this.dispose()
      error(err, 0)
    end

    local ok2, err2 = pcall(this._refresh, this)
    if not ok2 then
      this._error = err2
      pcall(function()
        this.ctx.logger:error(err2)
      end)
    end
  else
    self.uid = 0
    self.ctx = parent
    self.context = parent
    self.state = ACTIVE
    self.committed = proto.create(nil)
    self._runner = {
      epoch = "0",
      getOuterStack = getOuterStack,
      execute = function() end,
      collect = collect,
    }
    self.dispose = function()
      return this:restart()
    end
  end
end

function Fiber:leak(dispose)
  return self.ctx.fiber._disposables:delete(dispose)
end

Fiber.__get.name = function(self)
  local fiber = self
  repeat
    if fiber.runtime and fiber.runtime.name then
      return fiber.runtime.name
    end
    fiber = fiber.parent.fiber
  until fiber == fiber.parent.fiber
  return "root"
end

Fiber.__get.error = function(self)
  return self._error
end

function Fiber.__tostring(self)
  return "Fiber <" .. self.name .. ">"
end

-- `await ctx.use(x)`
function Fiber.__await(self)
  return self:await():await()
end

-- adoption by a Future (`use()` results are thenables upstream)
function Fiber.__future(self)
  return self:await()
end

function Fiber:assertActive()
  if self.uid ~= nil then
    return
  end
  error(errors.CordisError.new("INACTIVE_EFFECT"), 2)
end

-- run the component: called by the fiber's runner, in the fiber's scope
function Fiber:_executeComponent()
  local callback = self.runtime.callback
  local cfg = self:_resolve(self.config)
  local host = self.ctx.host
  if trace.isConstructor(callback) then
    -- not `callback.new`: a class may define a method named `new`
    local instance = construct(callback, self.ctx, cfg)
    local hooks = instance and require("cordis.registry").initHooks(instance)
    for _, hook in ipairs(hooks or {}) do
      hook()
    end
    local init = instance and proto.get(instance, S.init, instance)
    if init then
      return future.call(host, init, instance)
    end
    return nil
  else
    return future.call(host, callback, self.ctx, cfg)
  end
end

function Fiber:_resolve(config)
  local function resolve(cfg)
    return self.context:waterfall("internal/config", self, function()
      return cfg
    end)
  end
  if self._options.config then
    return self._options.config(self, config, resolve)
  end
  return resolve(config)
end

function Fiber:_execute(runner)
  local oldEpoch = runner.epoch
  local host = self.ctx.host
  return trace.composeError(function(info)
    local function safeCollect(dispose)
      if proto.callable(dispose) and not symbols.isSymbol(dispose) then
        runner.collect(dispose)
      elseif not symbols.isNullable(dispose) then
        invalidEffect()
      end
    end
    local effect = runner.execute(self)
    local mt = getmetatable(effect)
    if effect == nil or effect == symbols.null then
      return nil
    elseif proto.callable(effect) and not symbols.isSymbol(effect) and not future.isFuture(effect) then
      -- a function first, as upstream: an effect disposer is callable and a thenable
      return runner.collect(effect)
    elseif future.isThenable(effect) then
      return Future.resolve(host, effect):_next(safeCollect)
    elseif mt == Iter then
      info.traceback = errors.traceback()
      effect.fn(safeCollect, table.unpack(effect.args, 1, effect.args.n))
      return nil
    elseif mt == AsyncIter then
      local f, resolve, reject = Future.withResolvers(host)
      -- the iterator body runs in the context of its effect
      future.spawnIn(host, runner.scope, function()
        -- force async stack trace
        future.tick(host)
        info.traceback = errors.traceback()
        if runner.epoch ~= oldEpoch then
          return resolve(nil)
        end
        -- close the iterator so that its finally blocks (`<close>` variables) run
        local ok, err = pcall(effect.fn, function(dispose)
          safeCollect(dispose)
          if runner.epoch ~= oldEpoch then
            error(ABORT, 0)
          end
        end, table.unpack(effect.args, 1, effect.args.n))
        if ok or err == ABORT then
          resolve(nil)
        else
          reject(err)
        end
      end)
      return f
    else
      invalidEffect()
    end
  end, runner.getOuterStack)
end

function Fiber:effect(execute, lbl)
  lbl = lbl or "anonymous"
  self:assertActive()
  -- effects created during unload would escape the unload snapshot
  if self.state == UNLOADING then
    error(errors.CordisError.new("INACTIVE_EFFECT"), 2)
  end
  local this = self
  local host = self.ctx.host

  local disposables = {}
  local unregister
  -- a cleanup in progress; the wrapper stays registered until it
  -- settles, so an unload of the fiber joins it instead of skipping it
  local inFlight
  local function remove()
    if unregister then
      unregister()
      unregister = nil
    end
  end
  local function dispose()
    local list = disposables
    disposables = {}
    if #list == 0 then
      remove()
      return nil
    end
    -- publish a joinable completion before invoking any user disposer so a
    -- concurrent unload joins even a synchronously blocking (latch:wait) disposer
    local barrier, resolveBarrier, rejectBarrier = Future.withResolvers(host)
    inFlight = barrier
    local task
    local errCount, firstErr, errs = 0, nil, nil
    local function recordError(e)
      errCount = errCount + 1
      if errCount == 1 then
        firstErr = e
      else
        if not errs then
          errs = { firstErr == nil and symbols.null or firstErr }
        end
        errs[errCount] = e == nil and symbols.null or e
      end
    end
    for i = #list, 1, -1 do
      local d = list[i]
      if task then
        task = task:_next(function()
          return callDisposer(host, d)
        end, function(reason)
          recordError(reason)
          return callDisposer(host, d)
        end)
      else
        local ok, result = pcall(callDisposer, host, d)
        if not ok then
          recordError(result)
        elseif future.isThenable(result) then
          task = Future.resolve(host, result)
        end
      end
    end
    local function collectedError()
      if errCount == 1 then
        return firstErr
      end
      return errors.AggregateError.new(errs, "one or more disposers failed")
    end
    if not task then
      remove()
      if inFlight == barrier then
        inFlight = nil
      end
      if errCount > 0 then
        local e = collectedError()
        rejectBarrier(e)
        error(e, 0)
      end
      resolveBarrier(nil)
      return nil
    end
    task:_next(function()
      remove()
      if inFlight == barrier then
        inFlight = nil
      end
      if errCount > 0 then
        rejectBarrier(collectedError())
      else
        resolveBarrier(nil)
      end
    end, function(reason)
      recordError(reason)
      remove()
      if inFlight == barrier then
        inFlight = nil
      end
      rejectBarrier(collectedError())
    end)
    return barrier
  end

  local meta = { label = lbl, children = {} }
  -- disposers collected by this effect run in this context
  local scope = future.capture(host)
  local runner = {
    execute = function()
      return future.call(host, execute)
    end,
    epoch = true,
    scope = scope,
    collect = function(d)
      disposables[#disposables + 1] = remember(host, scope, d)
      this._disposables:delete(d)
      local m
      if getmetatable(d) == Disposer then
        m = rawget(d, S.effect)
      elseif type(d) == "table" then
        m = proto.get(d, S.effect, d)
      end
      if m then
        meta.children[#meta.children + 1] = m
      end
    end,
    getOuterStack = trace.buildOuterStack(),
  }

  local task
  local settingUp = true
  local deferredResolve, deferredReject, deferredFuture -- a dispose requested while setup is still running
  local cleanupTask

  local function ensureCleanup()
    if cleanupTask then
      return cleanupTask
    end
    local ok, res = pcall(dispose)
    if not ok then
      cleanupTask = Future.reject(host, res)
    elseif res then
      cleanupTask = res
    else
      cleanupTask = Future.resolve(host, nil)
    end
    return cleanupTask
  end

  local function afterTask()
    local pending
    pending = task:_next(ensureCleanup, ensureCleanup):_next(function(v)
      if inFlight == pending then
        inFlight = nil
      end
      return v
    end, function(e)
      if inFlight == pending then
        inFlight = nil
      end
      error(e, 0)
    end)
    inFlight = pending
    return pending
  end

  local wrapper = setmetatable({}, Disposer)
  rawset(wrapper, S.effect, meta)
  wrapper._dispose = function()
    if not runner.epoch then
      return inFlight
    end
    runner.epoch = false
    if settingUp then
      local f, resolve, reject = Future.withResolvers(host)
      deferredResolve, deferredReject, deferredFuture = resolve, reject, f
      inFlight = f
      return f
    end
    if task then
      return afterTask()
    end
    return dispose()
  end
  local function disposeAsync()
    if not runner.epoch then
      return inFlight
    end
    runner.epoch = false
    return dispose()
  end
  wrapper._future = function()
    return Future.resolve(host, task):_next(function()
      return disposeAsync
    end)
  end

  -- register the wrapper before running setup
  unregister = self._disposables:push(wrapper)

  local ok, result = pcall(self._execute, self, runner)
  settingUp = false
  if not ok then
    -- roll back what setup collected; an unload that started during setup
    -- waits for this rollback, whether or not it succeeds
    local resolveDeferred = deferredResolve
    local savedFuture = deferredFuture
    deferredResolve, deferredReject, deferredFuture = nil, nil, nil
    local okRollback, rollback = pcall(dispose)
    if inFlight == savedFuture then
      inFlight = nil
    end
    if resolveDeferred then
      resolveDeferred(okRollback and rollback or nil)
    end
    if not okRollback then
      error(rollback, 0)
    end
    error(result, 0)
  end
  task = result

  -- prevent unhandled rejection — both from `task` itself and from the
  -- disposer chain if it fails to settle cleanly.
  if task then
    task
      :_catch(function()
        return ensureCleanup()
      end)
      :_catch(function(e)
        pcall(function()
          this.ctx.logger:error(e)
        end)
      end)
  end

  if deferredResolve then
    local resolveDeferred, rejectDeferred = deferredResolve, deferredReject
    local savedFuture = deferredFuture
    deferredResolve, deferredReject, deferredFuture = nil, nil, nil
    if task then
      resolveDeferred(afterTask())
    else
      local okDispose, res = pcall(dispose)
      if inFlight == savedFuture then
        inFlight = nil
      end
      if okDispose then
        resolveDeferred(res)
      else
        rejectDeferred(res)
      end
    end
  end
  return wrapper
end

function Fiber:getEffects()
  local r = {}
  local n = 0
  local list = self._disposables
  local head = list.head
  if head then
    local node = head._tail
    while node do
      local d = node.v
      local m
      if getmetatable(d) == Disposer then
        m = rawget(d, S.effect)
      elseif type(d) == "table" then
        m = proto.get(d, S.effect, d)
      end
      if m then
        n = n + 1
        r[n] = m
      end
      node = node.prev
    end
  end
  local node = list.tail._head
  while node do
    local d = node.v
    local m
    if getmetatable(d) == Disposer then
      m = rawget(d, S.effect)
    elseif type(d) == "table" then
      m = proto.get(d, S.effect, d)
    end
    if m then
      n = n + 1
      r[n] = m
    end
    node = node.next
  end
  return r
end

function Fiber:pending()
  local out = {}
  if not self.inject then
    return out
  end
  for _, name in ipairs(proto.ownKeys(self.inject)) do
    if not self._store or self._store[name] == nil then
      out[#out + 1] = name
    end
  end
  return out
end

function Fiber:_getState()
  if self.uid == nil then
    return DISPOSED
  end
  if self._error ~= nil then
    return FAILED
  end
  if self._runner.epoch ~= INACTIVE then
    return ACTIVE
  end
  return PENDING
end

local function notifyProvisions(self)
  if self.provisions:size() == 0 then
    return
  end
  local errs
  for impl in self.provisions:pairs() do
    if self.ctx.reflect.store[impl.isolate] == impl then
      local ok, err = pcall(function()
        self.ctx:isolate(impl.name, impl.isolate).reflect:notify({ impl.name })
      end)
      if not ok then
        errs = errs or {}
        errs[#errs + 1] = err
      end
    end
  end
  if errs then
    error(#errs == 1 and errs[1] or errors.AggregateError.new(errs, "one or more provisions failed to notify"), 0)
  end
end

function Fiber:_updateState(callback)
  local oldState = self.state
  local s = callback()
  if s == nil then
    s = self:_getState()
  end
  self.state = s
  if oldState == self.state then
    return
  end
  if oldState == ACTIVE then
    local ok1, err1 = pcall(notifyProvisions, self)
    -- FIXME internal/fiber-info
    local ok2, err2 = pcall(self.context.emit, self.context, "internal/status", self, oldState)
    if not ok1 then
      error(err1, 0)
    end
    if not ok2 then
      error(err2, 0)
    end
    return
  end
  -- FIXME internal/fiber-info
  local ok, err = pcall(self.context.emit, self.context, "internal/status", self, oldState)
  if self.state == ACTIVE then
    notifyProvisions(self)
  end
  if not ok then
    error(err, 0)
  end
end

function Fiber:_checkImpl(name)
  local impl = self.ctx.reflect:getImpl(name, true)
  if not impl then
    return proto.delete(self._store, name)
  end
  if impl.check then
    self._checkSeq = self._checkSeq or {}
    local seq = (self._checkSeq[name] or 0) + 1
    self._checkSeq[name] = seq
    local ok, passed =
      pcall(future.runSync, impl.ctx.host, impl.scope, impl.check, trace.getTraceable(self.ctx, impl.value))
    if self._checkSeq[name] ~= seq then
      return
    end
    if not ok then
      pcall(function()
        impl.ctx.logger:error(passed)
      end)
      return proto.delete(self._store, name)
    end
    if not passed then
      return proto.delete(self._store, name)
    end
    local current = self.ctx.reflect:getImpl(name, true)
    if current ~= impl then
      if not current then
        return proto.delete(self._store, name)
      end
      return self:_checkImpl(name)
    end
  end
  self._store[name] = impl
end

function Fiber:_replace(runtime, inject)
  if runtime ~= self.runtime then
    self._remove()
    self._remove = runtime.fibers:push(self)
    self.runtime = runtime
  end
  self.inject = inject
  injectIndex.set(self, inject)
  -- `locate` reads runtime and inject
  resolveEpoch.bump()
  self.context:emit("internal/replace", self)
  self._store = proto.create(nil)
  for _, name in ipairs(proto.ownKeys(self.inject)) do
    self:_checkImpl(name)
  end
  self._error = nil
  self._revision = self._revision + 1
  self:_refresh()
end

function Fiber:_refresh()
  if self.uid == nil then
    return
  end
  local keys = proto.ownKeys(self.inject)
  local n = #keys
  if n == 0 then
    self:_setEpoch(tostring(self._revision))
    return
  end
  if n == 1 then
    local impl = self._store[keys[1]]
    self:_setEpoch(impl and (self._revision .. ":" .. impl.uid) or INACTIVE)
    return
  end
  local parts = { self._revision }
  for i = 1, n do
    local impl = self._store[keys[i]]
    if not impl then
      self:_setEpoch(INACTIVE)
      return
    end
    parts[i + 1] = impl.uid
  end
  self:_setEpoch(table.concat(parts, ":", 1, n + 1))
end

function Fiber:_setEpoch(epoch)
  local oldEpoch = self._runner.epoch
  if epoch == oldEpoch then
    return
  end
  -- a failed fiber only recovers through update(), which clears _error
  if self._error ~= nil then
    return
  end
  self._runner.epoch = epoch
  if self.inertia then
    return
  end
  self:_updateState(function()
    if epoch ~= INACTIVE and oldEpoch == INACTIVE then
      self.inertia = self:_reload()
      return LOADING
    else
      self.inertia = self:_unload()
      return UNLOADING
    end
  end)
end

function Fiber:_reload()
  local host = self.ctx.host
  self.committed = proto.create(nil, self._store)
  resolveEpoch.bump() -- `locate` reads committed
  for _, name in ipairs(proto.ownKeys(self.committed)) do
    local impl = self.committed[name]
    if impl.ctx.fiber ~= self then
      impl.dependents:add(self)
    end
  end
  local oldEpoch = self._runner.epoch
  -- one job hop before executing, like upstream's `await Promise.resolve()`
  return Future.resolve(host, nil)
    :_next(function()
      if self._runner.epoch == oldEpoch then
        -- the component (and the effect shapes it returns, which
        -- run user code) runs in the context of `use()`, not in the drainer
        local function execute()
          return self:_execute(self._runner)
        end
        -- in a drainer the component starts as its own task; keep its
        -- canceller while it runs (dispose of a LOADING fiber cancels it)
        if self._scope and future.inDrainer() then
          local f, cancel = future.start(host, self._scope, execute)
          if cancel then
            self._cancelApply = cancel
            return f:_next(function(v)
              self._cancelApply = nil
              return v
            end, function(err)
              self._cancelApply = nil
              error(err, 0)
            end)
          end
          return f
        end
        return future.run(host, self._scope, execute)
      end
    end)
    :_next(function() end, function(err)
      -- a cancelled apply is the dispose that cancelled it, not a failure
      if err == future.CANCELLED then
        return
      end
      -- A superseded load still needs cleanup, but cannot fail the new target.
      if self._runner.epoch == oldEpoch then
        self._error = err
        self._runner.epoch = INACTIVE
      end
      -- impl guarantees that the error is non-null (?)
      pcall(function()
        self.ctx.logger:error(err)
      end)
    end)
    :_next(function()
      self:_updateState(function()
        if self._runner.epoch == oldEpoch then
          self.inertia = nil
        else
          self.inertia = self:_unload()
          return UNLOADING
        end
      end)
    end)
end

function Fiber:_unload()
  local host = self.ctx.host
  local f, resolve, reject = Future.withResolvers(host)

  local function finish()
    -- release the providers waiting for this fiber to unload
    if self.committed then
      for _, name in ipairs(proto.ownKeys(self.committed)) do
        local impl = self.committed[name]
        if impl.dependents:delete(self) and impl.dependents:size() == 0 then
          if impl.release then
            impl.release()
          end
          local rel = impl.ctx.fiber._release
          if rel then
            rel()
          end
        end
      end
    end
    self.committed = nil
    resolveEpoch.bump() -- `locate` reads committed
    self:_updateState(function()
      if self._runner.epoch == INACTIVE then
        self.inertia = nil
      else
        self.inertia = self:_reload()
        return LOADING
      end
    end)
  end

  local function teardown()
    self._release = nil
    -- Disposers are started in the reverse order of registration and run
    -- concurrently; the order does not make one teardown wait for another.
    -- Effects that depend on one another use an effect iterator or a service.
    local list = self._disposables:clear()
    local tasks = {}
    for i = #list, 1, -1 do
      local d = list[i]
      -- one job hop before the disposer starts, like upstream's
      -- `await Promise.resolve()`; ordering against other teardown work
      -- (e.g. a provision withdrawn by a sibling disposer) depends on it
      tasks[#tasks + 1] = Future.resolve(host, nil)
        :_next(function()
          return trace.composeError(function()
            return callDisposer(host, d)
          end, self._runner.getOuterStack)
        end)
        :_catch(function(err)
          pcall(function()
            self.ctx.logger:error(err)
          end)
        end)
    end
    Future.all(host, tasks)
      :_next(function()
        finish()
        resolve(nil)
      end)
      :_catch(reject)
  end

  local function isDescendant(fiber, ancestor)
    local cur = fiber
    while cur do
      if rawequal(cur, ancestor) then
        return true
      end
      local nextFiber = cur.parent and cur.parent.fiber
      if not nextFiber or rawequal(nextFiber, cur) then
        return false
      end
      cur = nextFiber
    end
    return false
  end

  -- Dependents are notified when this fiber leaves ACTIVE, but may still
  -- access its services during their own teardown, so the effects of this
  -- fiber are only disposed after every dependent has unloaded.
  local function check()
    if self.uid == nil then
      local toDispose = {}
      for impl in self.provisions:pairs() do
        for dep in impl.dependents:pairs() do
          if dep.uid ~= nil and isDescendant(dep, self) then
            toDispose[#toDispose + 1] = dep
          end
        end
      end
      for _, dep in ipairs(toDispose) do
        if dep.uid ~= nil then
          pcall(dep.dispose, dep)
        end
      end
    end
    for impl in self.provisions:pairs() do
      if impl.dependents:size() > 0 then
        local released, release = Future.withResolvers(host)
        self._release = release
        released:_next(check):_catch(reject)
        return
      end
    end
    teardown()
  end
  check()
  return f
end

function Fiber:await()
  local host = self.ctx.host
  local this = self
  local f, _, reject = Future.withResolvers(host)
  local function loop()
    if this.inertia then
      this.inertia:_next(loop, reject)
    elseif this._error ~= nil then
      reject(this._error)
    else
      -- the fiber itself is a thenable: fulfill without adopting it again
      future.fulfill(f, this)
    end
  end
  Future.resolve(host, nil):_next(loop)
  return f
end

function Fiber:restart()
  local fiber = self.ctx.fiber
  local host = self.ctx.host
  local ok, err = pcall(function()
    fiber:assertActive()
    fiber._revision = fiber._revision + 1
    fiber:_refresh()
  end)
  if not ok then
    return Future.reject(host, err)
  end
  return fiber:await():_next(function() end)
end

function Fiber:update(config, noSave)
  local fiber = self.ctx.fiber
  local host = self.ctx.host
  fiber:assertActive()
  local loaded = fiber._runner.epoch ~= INACTIVE
  local resolved
  if loaded then
    resolved = fiber:_resolve(config)
  end
  if not noSave and fiber._options.commit then
    fiber._options.commit(fiber, config)
  end
  fiber.config = config
  fiber._error = nil
  local function restart()
    return fiber:restart()
  end
  local result
  if loaded then
    result = fiber.context:waterfall(fiber, "internal/update", resolved, restart)
  else
    result = restart()
  end
  -- a listener may veto the restart, in which case there is nothing to await
  if result == nil then
    return nil
  end
  local task = Future.resolve(host, result)
  -- the failure is already reported by the fiber, so mark it handled here
  task:_catch(function() end)
  return task
end

M.INACTIVE = INACTIVE
return M
