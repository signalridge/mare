-- Ported from cordis packages/core/src/events.ts (MIT, (c) Shigma)
local proto = require("cordis.proto")
local class = require("cordis.class").class
local symbols = require("cordis.symbols")
local S = symbols.symbols
local errors = require("cordis.error")
local future = require("cordis.future")
local Future = future.Future
local DisposableList = require("cordis.list")

local M = {}

local EMPTY = {}
local type = type
local getmetatable = getmetatable
local Symbol = symbols._Symbol
local NULL = symbols.null
local FILTER = S.filter
local protoGetWatched = proto._getWatched
local protoState = proto._state
local rawget = rawget

proto._watchKey(FILTER, 4)

-- `null` is `nil` or `cordis.null`
local function isBailed(value)
  return value ~= nil and value ~= false and value ~= NULL
end
M.isBailed = isBailed

local EventsService = class("EventsService")
M.EventsService = EventsService

local function unwrapEvents(self)
  local s = protoState[self]
  if s and s.handler then
    local raw = s.target
    if getmetatable(raw) == EventsService then
      local tc = s.context
      local ctx = tc and tc.tracker and tc.tracker.property == "ctx" and tc.useSite or self.ctx
      return raw, ctx
    end
  elseif getmetatable(self) == EventsService then
    return self, rawget(self, "ctx")
  end
  return self, self.ctx
end

function EventsService:constructor(ctx)
  self.ctx = ctx
  self._hooks = {}
  self._snapshots = {}
  rawset(self, S.tracker, { property = "ctx", noShadow = true })

  self:on("internal/listener", function(this, name, listener, options)
    if name == "internal/update" and not options.global then
      local fiber = this.fiber
      local hooks = fiber._hooks["internal/update"]
      if not hooks then
        hooks = DisposableList.new()
        fiber._hooks["internal/update"] = hooks
      end
      local method = options.prepend and "unshift" or "push"
      return fiber:effect(function()
        local removed = false
        local dispose = hooks[method](hooks, function(thisFiber, config, nextFn)
          if removed then
            return nextFn()
          end
          return listener(thisFiber, config, nextFn)
        end)
        return future.internal(function()
          removed = true
          return dispose()
        end)
      end, 'ctx.on("internal/update")')
    end
  end)
  local lh = self._hooks["internal/listener"]
  self._defaultListenerHook = lh and lh[1] or nil

  self:on("internal/update", function(fiber, config, nextFn)
    local cbs = fiber._hooks["internal/update"] and fiber._hooks["internal/update"]:values() or {}
    local i = 0
    local function dispatch()
      i = i + 1
      local cb = cbs[i]
      if not cb then
        return nextFn()
      end
      local called = false
      local function stepNext()
        if called then
          error(errors.Error.new("next() called multiple times"), 2)
        end
        called = true
        return dispatch()
      end
      return cb(fiber, config, stepNext)
    end
    return dispatch()
  end, { global = true, prepend = true })

  self:on("internal/config", function(fiber, nextFn)
    return require("cordis.config").resolveConfig(fiber.runtime, nextFn())
  end)
end

local function isThisArg(x)
  local t = type(x)
  if t == "function" then
    return true
  end
  return t == "table" and getmetatable(x) ~= Symbol and x ~= NULL
end

local function getSnapshot(self, name, bucket)
  local hooks = self._snapshots[name]
  if not hooks then
    hooks = {}
    for i = 1, #bucket do
      hooks[i] = bucket[i]
    end
    self._snapshots[name] = hooks
  end
  return hooks
end

-- returns thisArg, hooks, name, args (packed, without thisArg and name).
-- Unfiltered hooks may return a shared cached array (read-only within Cordis);
-- the public `resolve` copies it before returning.
local function resolveInternal(self, mode, ...)
  local args = table.pack(...)
  local first = 1
  local thisArg
  if isThisArg(args[1]) then
    thisArg = args[1]
    first = 2
  end
  local name = args[first]
  local rest = table.pack(table.unpack(args, first + 1, args.n))
  if self._hooks["internal/dispatch"] and (type(name) ~= "string" or name:sub(1, 9) ~= "internal/") then
    local list = {}
    for i = 1, rest.n do
      list[i] = rest[i]
    end
    list.n = rest.n
    self:emit("internal/dispatch", mode, name, list, thisArg)
  end
  local filter = thisArg ~= nil and protoGetWatched(thisArg, FILTER) or nil
  local bucket = self._hooks[name]
  if not bucket then
    return thisArg, EMPTY, name, rest
  end
  local hooks
  if not filter then
    hooks = getSnapshot(self, name, bucket)
  else
    hooks = {}
    for _, hook in ipairs(bucket) do
      if hook.global or filter(thisArg, hook.ctx) then
        hooks[#hooks + 1] = hook
      end
    end
  end
  return thisArg, hooks, name, rest
end

-- adapter-facing resolution returns an independent hook array
function EventsService:resolve(mode, ...)
  local thisArg, hooks, name, rest = resolveInternal(self, mode, ...)
  local copy = {}
  for i = 1, #hooks do
    copy[i] = hooks[i]
  end
  return thisArg, copy, name, rest
end

function EventsService:listeners(name)
  local bucket = self._hooks[name]
  if not bucket then
    return {}
  end
  local copy = {}
  for i = 1, #bucket do
    copy[i] = bucket[i]
  end
  return copy
end

function EventsService:listenerCount(name)
  local bucket = self._hooks[name]
  return bucket and #bucket or 0
end

local function invoke(host, hook, thisArg, args, ...)
  local all = {}
  local n = 0
  if thisArg ~= nil then
    n = 1
    all[1] = thisArg
  end
  for i = 1, args.n do
    all[n + i] = args[i]
  end
  n = n + args.n
  local extra = select("#", ...)
  for i = 1, extra do
    all[n + i] = (select(i, ...))
  end
  n = n + extra
  -- a listener runs in the context it was registered in, called
  -- synchronously as upstream (`scope.run`) so dispatch results are unchanged
  return future.runSync(host, hook.scope, hook.callback, table.unpack(all, 1, n))
end

---@deprecated
function EventsService:dispatch(mode, args)
  local thisArg, hooks = resolveInternal(self, mode, table.unpack(args, 1, args.n or #args))
  local host = self.ctx.host
  local result = {}
  for i, hook in ipairs(hooks) do
    result[i] = function(...)
      if hook.removed then
        if mode == "waterfall" then
          local n = select("#", ...)
          local last = select(n, ...)
          return last()
        end
        return nil
      end
      -- upstream (events.ts:96-106) passes only the closure's own arguments
      return invoke(host, hook, thisArg, table.pack(...))
    end
  end
  return result
end

local function label(name)
  if symbols.isSymbol(name) then
    return tostring(name)
  end
  return string.format('"%s"', tostring(name))
end

local function parallel(self, ...)
  local thisArg, hooks, name, args = resolveInternal(self, "parallel", ...)
  local host = self.ctx.host
  local list = {}
  for i, hook in ipairs(hooks) do
    if hook.removed then
      list[i] = Future.resolve(host, nil)
    elseif hook.scope then
      -- each listener starts as its own task in its registrant's
      -- context, so one that blocks natively does not delay the others
      local ok, result = pcall(future.start, host, hook.scope, function()
        -- tasks start in order: one removed by an earlier listener is skipped
        if hook.removed then
          return nil
        end
        return invoke(host, hook, thisArg, args)
      end)
      if ok then
        list[i] = Future.resolve(host, result)
      else
        list[i] = Future.reject(host, result)
      end
    else
      local ok, result = pcall(invoke, host, hook, thisArg, args)
      if ok then
        list[i] = Future.resolve(host, result)
      else
        list[i] = Future.reject(host, result)
      end
    end
  end
  return Future.allSettled(host, list):_next(function(results)
    -- count rejections by status: a rejection may carry no reason (nil),
    -- which is kept as `cordis.null` (events.ts:113)
    local errs = { n = 0 }
    for _, r in ipairs(results) do
      if r.status == "rejected" then
        errs.n = errs.n + 1
        local reason = r.reason
        if reason == nil then
          reason = symbols.null
        end
        errs[errs.n] = reason
      end
    end
    if errs.n == 0 then
      return nil
    end
    error(errors.AggregateError.new(errs, "one or more listeners of event " .. label(name) .. " failed"), 0)
  end)
end

local function emitNoThis(raw, ctx, name, ...)
  local bucket = raw._hooks[name]
  if not bucket then
    return
  end
  local hooks = getSnapshot(raw, name, bucket)
  local host = ctx.host
  for i = 1, #hooks do
    local hook = hooks[i]
    if not hook.removed then
      future.runSync(host, hook.scope, hook.callback, ...)
    end
  end
end

local function emitWithThis(raw, ctx, thisArg, name, ...)
  local bucket = raw._hooks[name]
  if not bucket then
    return
  end
  local filter = protoGetWatched(thisArg, FILTER)
  local hooks
  if not filter then
    hooks = getSnapshot(raw, name, bucket)
  else
    hooks = {}
    for i = 1, #bucket do
      local hook = bucket[i]
      if hook.global or filter(thisArg, hook.ctx) then
        hooks[#hooks + 1] = hook
      end
    end
  end
  local host = ctx.host
  for i = 1, #hooks do
    local hook = hooks[i]
    if not hook.removed then
      future.runSync(host, hook.scope, hook.callback, thisArg, ...)
    end
  end
end

function EventsService:emit(...)
  local raw, ctx = unwrapEvents(self)
  if not raw._hooks["internal/dispatch"] then
    local first = ...
    if isThisArg(first) then
      return emitWithThis(raw, ctx, ...)
    end
    return emitNoThis(raw, ctx, ...)
  end
  local thisArg, hooks, _, args = resolveInternal(self, "emit", ...)
  local host = ctx.host
  for _, hook in ipairs(hooks) do
    if not hook.removed then
      invoke(host, hook, thisArg, args)
    end
  end
end

-- emit on a teardown path: every listener runs even if another one fails;
-- errors are reported through `onError`
function EventsService:_emitIsolated(onError, ...)
  local thisArg, hooks, _, args = resolveInternal(self, "emit", ...)
  local host = self.ctx.host
  for _, hook in ipairs(hooks) do
    if not hook.removed then
      local ok, err = pcall(invoke, host, hook, thisArg, args)
      if not ok then
        onError(err)
      end
    end
  end
end

local function serial(self, ...)
  local thisArg, hooks, _, args = resolveInternal(self, "serial", ...)
  local host = self.ctx.host
  local f, resolve, reject = Future.withResolvers(host)
  local i = 0
  local function step()
    while true do
      i = i + 1
      local hook = hooks[i]
      if not hook then
        return resolve(nil)
      end
      if not hook.removed then
        local ok, result = pcall(invoke, host, hook, thisArg, args)
        if not ok then
          return reject(result)
        end
        -- `await` every result, as upstream: the next listener runs in a later
        -- job even after a synchronous result (events.ts:126-133)
        Future.resolve(host, result):_next(function(value)
          if isBailed(value) then
            return resolve(value)
          end
          step()
        end, reject)
        return
      end
    end
  end
  step()
  return f
end

local function bailNoThis(self, name, ...)
  local bucket = self._hooks[name]
  if not bucket then
    return
  end
  local hooks = getSnapshot(self, name, bucket)
  local host = self.ctx.host
  for i = 1, #hooks do
    local hook = hooks[i]
    if not hook.removed then
      local result = future.runSync(host, hook.scope, hook.callback, ...)
      if isBailed(result) then
        return result
      end
    end
  end
end

local function bailWithThis(self, thisArg, name, ...)
  local bucket = self._hooks[name]
  if not bucket then
    return
  end
  local filter = protoGetWatched(thisArg, FILTER)
  local hooks
  if not filter then
    hooks = getSnapshot(self, name, bucket)
  else
    hooks = {}
    for i = 1, #bucket do
      local hook = bucket[i]
      if hook.global or filter(thisArg, hook.ctx) then
        hooks[#hooks + 1] = hook
      end
    end
  end
  local host = self.ctx.host
  for i = 1, #hooks do
    local hook = hooks[i]
    if not hook.removed then
      local result = future.runSync(host, hook.scope, hook.callback, thisArg, ...)
      if isBailed(result) then
        return result
      end
    end
  end
end

function EventsService:bail(...)
  if not self._hooks["internal/dispatch"] then
    local first = ...
    if isThisArg(first) then
      return bailWithThis(self, ...)
    end
    return bailNoThis(self, ...)
  end
  local thisArg, hooks, _, args = resolveInternal(self, "bail", ...)
  local host = self.ctx.host
  for _, hook in ipairs(hooks) do
    if not hook.removed then
      local result = invoke(host, hook, thisArg, args)
      if isBailed(result) then
        return result
      end
    end
  end
end

function EventsService:waterfall(...)
  local thisArg, hooks, _, args = resolveInternal(self, "waterfall", ...)
  local host = self.ctx.host
  local inner = args[args.n]
  args = table.pack(table.unpack(args, 1, args.n - 1))
  -- the terminal `inner` belongs to the caller of `waterfall`; it
  -- runs in the caller's context, not in the last listener's
  local callerScope = future.capture(host)
  local index = 0
  local dispatch
  dispatch = function()
    index = index + 1
    while hooks[index] and hooks[index].removed do
      index = index + 1
    end
    local hook = hooks[index]
    if not hook then
      return future.runSync(host, callerScope, inner)
    end
    local called = false
    local function nextFn()
      if called then
        error(errors.Error.new("next() called multiple times"), 2)
      end
      called = true
      return dispatch()
    end
    return invoke(host, hook, thisArg, args, nextFn)
  end
  return dispatch()
end

-- `parallel` / `serial` are async upstream (events.ts:108, :126): an error while
-- resolving the dispatch rejects the returned Future
function EventsService:parallel(...)
  local ok, result = pcall(parallel, self, ...)
  if ok then
    return result
  end
  return Future.reject(self.ctx.host, result)
end

function EventsService:serial(...)
  local ok, result = pcall(serial, self, ...)
  if ok then
    return result
  end
  return Future.reject(self.ctx.host, result)
end

local function registerInternal(raw, ctx, lbl, name, callback, options)
  local host = ctx.host
  local prepend = options and options.prepend or nil
  local global = options and options.global or nil
  local allHooks = raw._hooks
  local snapshots = raw._snapshots
  return ctx.fiber:effect(function()
    local hooks = allHooks[name]
    if not hooks then
      hooks = {}
      allHooks[name] = hooks
    end
    local hook = {
      ctx = ctx,
      callback = callback,
      prepend = prepend,
      global = global,
      scope = future.capture(host),
    }
    if prepend then
      table.insert(hooks, 1, hook)
    else
      hooks[#hooks + 1] = hook
    end
    snapshots[name] = nil
    return future.internal(function()
      return raw:unregister(name, hook)
    end)
  end, lbl)
end

function EventsService:register(lbl, name, callback, options)
  local raw, ctx = unwrapEvents(self)
  return registerInternal(raw, ctx, lbl, name, callback, options)
end

function EventsService:unregister(name, hook)
  hook.removed = true
  local raw = unwrapEvents(self)
  local allHooks = raw._hooks
  local hooks = allHooks[name]
  if not hooks then
    return
  end
  for i, h in ipairs(hooks) do
    if h == hook then
      table.remove(hooks, i)
      if #hooks == 0 then
        allHooks[name] = nil
      end
      raw._snapshots[name] = nil
      return true
    end
  end
end

function EventsService:on(name, listener, options)
  local raw, ctx = unwrapEvents(self)
  -- handle special events
  ctx.fiber:assertActive()
  listener = ctx.reflect:bind(listener)
  local lhooks = raw._hooks["internal/listener"]
  if lhooks ~= nil and (#lhooks ~= 1 or lhooks[1] ~= raw._defaultListenerHook or name == "internal/update") then
    if type(options) ~= "table" then
      options = { prepend = options }
    end
    -- a listener handed to `internal/listener` (e.g. the per-fiber
    -- `internal/update` hooks) still runs in its registrant's context
    local host = ctx.host
    local scope = future.capture(host)
    local special = listener
    if scope then
      local bound = listener
      special = function(...)
        return future.runSync(host, scope, bound, ...)
      end
    end
    local result = self:bail(ctx, "internal/listener", name, special, options)
    if result then
      return result
    end
  elseif type(options) ~= "table" then
    options = options and { prepend = options } or EMPTY
  end

  local lbl
  if type(name) == "string" then
    lbl = string.format("ctx.on(%q)", name)
  else
    lbl = "ctx.on(" .. tostring(name) .. ")"
  end
  if raw == self or getmetatable(raw) == EventsService then
    return registerInternal(raw, ctx, lbl, name, listener, options)
  end
  return self:register(lbl, name, listener, options)
end

function EventsService:once(name, listener, options)
  local host = self.ctx.host
  local scope = future.capture(host)
  local dispose
  dispose = self:on(name, function(...)
    dispose()
    return future.runSync(host, scope, listener, ...)
  end, options)
  return dispose
end

return M
