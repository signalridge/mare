-- Ported from cordis packages/core/src/reflect.ts (MIT, (c) Shigma)
local proto = require("cordis.proto")
local classlib = require("cordis.class")
local class = classlib.class
local symbols = require("cordis.symbols")
local S = symbols.symbols
local trace = require("cordis.trace")
local errors = require("cordis.error")
local future = require("cordis.future")
local Future = future.Future
local Set = require("cordis.ordered").Set
local injectIndex = require("cordis.inject_index")
local resolveEpoch = require("cordis.resolve_epoch")

local get, set, has = proto.get, proto.set, proto.has
local protoState = proto._state
local protoEpoch = proto._protoEpoch
local ctxVersion = proto._ctxVersion
local getUntilHandler = proto._getUntilHandler
local getTraceable = trace.getTraceable
local createTraceable = trace.createTraceable
local traceables = trace._traceables
local classes = classlib._classes
local classLookup = classlib.lookup
local classHasGetters = classlib._hasGetters
local Symbol = symbols._Symbol

proto._watchKey(S.shadow, 1)
proto._watchKey(S.receiver, 2)
local type = type
local byte = string.byte
local tointeger = math.tointeger
local tonumber = tonumber
local tostring = tostring
local rawget = rawget
local rawequal = rawequal
local getmetatable = getmetatable
local setmetatable = setmetatable

local WEAK_KEYS = { __mode = "k" }
local mixinGetters = setmetatable({}, WEAK_KEYS)
local mixinBound = setmetatable({}, WEAK_KEYS)
local mixinTargets = {}
local mixinEpoch = 1

local M = {}

local FiberState -- resolved lazily (fiber.lua requires this module's users)
local function state()
  FiberState = FiberState or require("cordis.fiber").FiberState
  return FiberState
end

-- An access creates this placeholder before its outcome is known.
local function createAccessError(message)
  return { message = message, placeholder = true }
end

-- Construct the error raised by a failed access from its placeholder.
local function enhanceAccessError(placeholder)
  return errors.Error.new(placeholder.message)
end

-- - is a symbol (or not a string)
-- - is a number string (0, 1, 2, ...)
-- - starts with `_`
-- (the JS reserved words `then` and `prototype` are not special)
local function isSpecialProperty(prop)
  if type(prop) ~= "string" then
    return true
  end
  -- the first byte decides without building a substring; only a property that can start a
  -- canonical integer (`0`-`9` or `-`) needs conversion
  local b = byte(prop, 1)
  if b == nil or b > 95 then
    return false
  end
  if b == 95 then -- "_"
    return true
  end
  if (b < 48 or b > 57) and b ~= 45 then
    return false
  end
  local n = tointeger(tonumber(prop))
  return n ~= nil and tostring(n) == prop
end
M.isSpecialProperty = isSpecialProperty

local function rethrow(e, placeholder)
  if rawequal(e, placeholder) then
    error(enhanceAccessError(e), 0)
  end
  error(e, 0)
end

-- Resolve an access to service `name` from the def site `ctx` by walking up its fibers.
-- Returns `(found, fiber, impl)` without allocating a result table.
local function locateValues(ctx, name)
  local iso = ctx[S.isolate]
  local key = iso[name]
  local tbl = iso
  local fiber = ctx.fiber
  while true do
    local impl
    local provisions = fiber.provisions
    if provisions:size() > 0 then
      local node = provisions._map._head
      while node do
        local provision = node.k
        if provision.name == name and provision.ctx[S.isolate][name] == key then
          impl = provision
        end
        node = node.next
      end
    end
    if impl then
      return true, nil, impl
    end
    local fiberIso = fiber.ctx[S.isolate]
    while not rawequal(tbl, fiberIso) do
      if tbl == nil or proto.hasOwn(tbl, name) then
        return false, nil, nil
      end
      tbl = proto.getPrototypeOf(tbl)
    end
    impl = fiber.committed and fiber.committed[name]
    if impl or has(fiber.inject, name) then
      return true, fiber, impl
    end
    if not fiber.runtime then
      return false, nil, nil
    end
    fiber = fiber.parent.fiber
  end
end

-- `fiber` is the fiber whose `inject` admits the access, and `impl` is the impl the access reaches.
-- - `{ impl }`: a fiber provides the impl itself in the same isolation.
-- - `{ fiber, impl }`: the impl comes from the committed view of `fiber`.
-- - `{ fiber }`: `fiber` declares the service but is inactive.
-- - `nil`: no fiber declares the service, or the walk reaches a context that isolates `name` again.
function M.locate(ctx, name)
  local found, fiber, impl = locateValues(ctx, name)
  if not found then
    return nil
  end
  if fiber then
    return { fiber = fiber, impl = impl }
  end
  return { impl = impl }
end

-- the provision a property read through a context resolved to, per reader
-- (`ctx`, weak) and property: { target, epoch, hooks, impl }. Reused while the
-- resolution epoch is unchanged (cordis/resolve_epoch.lua lists what bumps it)
-- and no `internal/get` listener exists; the value is read from the provision
-- and the traceable made per read. An entry holds nothing strongly (weak
-- values): a provision gone from the store, the fibers and every committed set
-- is not kept alive by a reader that read it.
local resolved = setmetatable({}, WEAK_KEYS)
local WEAK_VALUES = { __mode = "v" }

local function emitInternalProvide(ctx, name, value)
  local target = ctx
  while proto.isProxy(target) do
    target = proto.proxyTarget(target)
  end
  if not has(target, "events") then
    return
  end
  local events = get(target, "events", ctx)
  local hooks = events and events._hooks["internal/provide"]
  if hooks and #hooks > 0 then
    events:emit(ctx, "internal/provide", name, value)
  end
end

-- Read `prop` directly from `target`'s plain prototype chain (`target` -> `ContextProto` -> nil)
-- without calling `proto.has` + `proto.get` twice. Falls back to `has`/`get` if a proxy or
-- non-proto object appears on the chain.
local function getTargetProp(target, prop, ctx)
  local s = protoState[target]
  while s and not s.handler do
    local k1 = s.k1
    if k1 ~= nil and k1 == prop then
      return s.v1, true
    end
    local kv = s.kv
    if kv then
      local v = kv[prop]
      if v ~= nil then
        return v, true
      end
    end
    local ps = s.pstate
    if ps then
      s = ps
    else
      local p = s.proto
      if p == nil then
        return nil, false
      end
      if has(p, prop) then
        return get(p, prop, ctx), true
      end
      return nil, false
    end
  end
  if has(target, prop) then
    return get(target, prop, ctx), true
  end
  return nil, false
end

local function storeResolved(ctx, prop, a1, a2, a3, a4, a5, a6, a7, a8, a9, a10)
  local byProp = resolved[ctx]
  if not byProp then
    byProp = {}
    resolved[ctx] = byProp
  end
  local entry = byProp[prop]
  if entry then
    entry[1], entry[2], entry[3], entry[4], entry[5] = a1, a2, a3, a4, a5
    if a5 == 3 or a5 == 4 then
      entry[6], entry[7], entry[8], entry[9], entry[10] = a6, a7, a8, a9, a10
    end
  elseif a5 == 3 or a5 == 4 then
    byProp[prop] = setmetatable({ a1, a2, a3, a4, a5, a6, a7, a8, a9, a10 }, WEAK_VALUES)
  else
    byProp[prop] = setmetatable({ a1, a2, a3, a4, a5 }, WEAK_VALUES)
  end
end

local TRACKER, SHADOW = S.tracker, S.shadow

local function readPlainState(s, k)
  local k1 = s.k1
  if k1 ~= nil and k1 == k then
    return s.v1, true
  end
  local kv = s.kv
  if kv then
    local v = kv[k]
    if v ~= nil then
      return v, true
    end
  end
  return nil, false
end

M.handler = {
  get = function(target, prop, ctx)
    if type(prop) ~= "string" then
      return get(target, prop, ctx)
    end
    -- Fast path for root target own properties (`fiber`, `host`, `events`, `reflect`, `registry`)
    -- and direct `Context.prototype` methods (`extend`, `isolate`, `intercept`, `close`),
    -- avoiding `resolved` table allocation and weak-table churn on every child context.
    local tstate = protoState[target]
    local plainTarget = tstate ~= nil and not tstate.handler
    local tkv = plainTarget and tstate.kv or nil
    local tp = plainTarget and tstate.pstate or nil
    local terminalTp = tp ~= nil and not tp.handler and tp.pstate == nil and tp.proto == nil
    local fastChecked = plainTarget and ((tp == nil and tstate.proto == nil) or terminalTp)
    if plainTarget then
      local tv, found = readPlainState(tstate, prop)
      if found then
        if type(tv) ~= "table" or isSpecialProperty(prop) then
          return tv
        end
        local mt = getmetatable(tv)
        if mt == nil then
          if tv[SHADOW] == nil and tv[TRACKER] == nil then
            return tv
          end
        elseif classes[mt] then
          if rawget(tv, SHADOW) == nil and rawget(tv, TRACKER) == nil and classLookup(mt, TRACKER, tv) == nil then
            return tv
          end
        end
        return getTraceable(ctx, tv)
      end
      if terminalTp then
        local pv, foundTp = readPlainState(tp, prop)
        if foundTp then
          if type(pv) ~= "table" or isSpecialProperty(prop) then
            return pv
          end
          return getTraceable(ctx, pv)
        end
      end
    end
    -- a remembered read first (only non-special properties are remembered)
    local byProp = resolved[ctx]
    local hit = byProp and byProp[prop]
    if hit and hit[2] == resolveEpoch.n and rawequal(hit[1], target) then
      local kind = hit[5]
      if kind == 1 then
        local impl = hit[4]
        if impl then
          local hooks = hit[3]["internal/get"]
          if hooks == nil or #hooks == 0 then
            return getTraceable(ctx, impl.value)
          end
        end
      elseif kind == 3 then
        local bound = hit[3]
        local rawSvc = hit[4]
        local origFn = hit[7]
        if bound and rawSvc and origFn then
          local propKey = hit[6]
          if hit[9] then
            if rawequal(rawget(rawSvc, propKey), origFn) then
              return bound
            end
          else
            local cls = hit[8]
            local getters = cls and classHasGetters[cls]
            if
              rawget(rawSvc, propKey) == nil
              and cls
              and rawequal(rawget(cls, propKey), origFn)
              and (not getters or not getters[propKey])
            then
              return bound
            end
          end
        end
      end
    end
    if isSpecialProperty(prop) then
      return get(target, prop, ctx)
    end
    -- the state this read sees; it is remembered only if nothing changed while resolving
    local epoch = resolveEpoch.n
    if not fastChecked then
      local targetVal, foundOnTarget = getTargetProp(target, prop, ctx)
      if foundOnTarget then
        return type(targetVal) == "table" and getTraceable(ctx, targetVal) or targetVal
      end
    end

    local tReflect = tkv and tkv.reflect or target.reflect
    -- an accessor (mixins such as `emit`, `waterfall`) is
    -- called without building a closure for the pcall
    local def = tReflect.props[prop]
    if def and def.type == "accessor" then
      local getter = def.get
      local meta = mixinGetters[getter]
      if meta then
        local receiver = getUntilHandler(ctx, S.receiver, M.handler)
        if receiver == nil then
          if fastChecked then
            receiver = readPlainState(tstate, S.receiver)
            if receiver == nil and terminalTp then
              receiver = readPlainState(tp, S.receiver)
            end
          else
            receiver = get(target, S.receiver, ctx)
          end
        end
        local res = getter(ctx, receiver)
        -- cache bound mixin methods on non-service root/plain-chain targets (e.g. events,
        -- reflect, registry, fiber) when there is no overriding receiver
        if receiver == nil and type(res) == "function" and resolveEpoch.n == epoch and not tReflect.props[meta.key] then
          local rawSvc = (tkv and tkv[meta.key]) or ctx[meta.key]
          local ss = protoState[rawSvc]
          while ss and ss.handler do
            rawSvc = ss.target
            ss = protoState[rawSvc]
          end
          if type(rawSvc) == "table" then
            local mt = getmetatable(rawSvc)
            if classes[mt] then
              local propKey = meta.propKey
              local ownFn = rawget(rawSvc, propKey)
              if type(ownFn) == "function" then
                storeResolved(ctx, prop, target, epoch, res, rawSvc, 3, propKey, ownFn, mt, true)
              elseif ownFn == nil then
                local getters = classHasGetters[mt]
                local clsFn = (not getters or not getters[propKey]) and rawget(mt, propKey) or nil
                if type(clsFn) == "function" then
                  storeResolved(ctx, prop, target, epoch, res, rawSvc, 3, propKey, clsFn, mt, false)
                end
              end
            end
          end
        end
        return res
      end
      local err = createAccessError(string.format('cannot get property "%s" without inject', prop))
      local ok, result = pcall(getter, ctx, ctx[S.receiver], err)
      if not ok then
        rethrow(result, err)
      end
      return result
    end

    -- a fiber-less def site cannot declare `inject` at all, so it keeps the unchecked root access.
    local shadow = getUntilHandler(ctx, S.shadow, M.handler)
    if shadow == nil then
      if fastChecked then
        shadow = readPlainState(tstate, S.shadow)
        if shadow == nil and terminalTp then
          shadow = readPlainState(tp, S.shadow)
        end
      else
        shadow = get(target, S.shadow, ctx)
      end
    end
    local defSite = shadow or ctx
    if not defSite.fiber.runtime then
      return ctx.reflect:get(prop, false)
    end

    local eventsSvc = tkv and tkv.events
    local allHooks = eventsSvc and eventsSvc._hooks or ctx.events._hooks
    local hooks = allHooks["internal/get"]
    -- with no `internal/get` listener the waterfall's result is the fallback's (an
    -- internal event is never dispatched to `internal/dispatch`), so resolve directly
    -- without allocating closures or a placeholder error
    if hooks == nil or #hooks == 0 then
      local found, _, foundImpl = locateValues(defSite, prop)
      if not found then
        error(errors.Error.new(string.format('cannot get property "%s" without inject', prop)), 0)
      end
      if foundImpl then
        if resolveEpoch.n == epoch then
          storeResolved(ctx, prop, target, epoch, allHooks, foundImpl, 1)
        end
        return getTraceable(ctx, foundImpl.value)
      end
      error(errors.Error.new(string.format('cannot get required service "%s" in inactive context', prop)), 0)
    end

    local err = createAccessError(string.format('cannot get property "%s" without inject', prop))
    local ok, result = pcall(function()
      local function fallback()
        local found, _, foundImpl = locateValues(defSite, prop)
        if not found then
          error(err, 0)
        end
        if foundImpl then
          -- remember what the read resolved to (only from the plain lookup, never from an
          -- `internal/get` listener's answer)
          if resolveEpoch.n == epoch then
            storeResolved(ctx, prop, target, epoch, allHooks, foundImpl, 1)
          end
          return getTraceable(ctx, foundImpl.value)
        end
        err.message = string.format('cannot get required service "%s" in inactive context', prop)
        error(err, 0)
      end
      return ctx.events:waterfall("internal/get", ctx, prop, err, fallback)
    end)
    if not ok then
      rethrow(result, err)
    end
    return result
  end,

  set = function(target, prop, value, ctx)
    if isSpecialProperty(prop) then
      return set(target, prop, value, ctx)
    end

    local err = createAccessError(string.format('cannot set property "%s" without provide', prop))
    local def = target.reflect.props[prop]
    if not def then
      if not ctx.fiber.runtime then
        emitInternalProvide(ctx, prop, value)
        return set(target, prop, value, ctx)
      end
      error(enhanceAccessError(err), 0)
    end

    local ok, result = pcall(function()
      if def.type == "accessor" then
        if not def.set then
          return false
        end
        return def.set(ctx, value, ctx[S.receiver], err)
      end
      return ctx.events:waterfall("internal/set", ctx, prop, value, err, function()
        return ctx.reflect:set(prop, value, err)
      end)
    end)
    if not ok then
      rethrow(result, err)
    end
    return result
  end,

  has = function(target, prop)
    if isSpecialProperty(prop) then
      return has(target, prop)
    end
    if has(target, prop) then
      return true
    end
    return target.reflect.props[prop] ~= nil
  end,
}

-- Specialized handler for `Context.new` root proxies via `proto._proxyWithContext`.
-- Keeps direct references to `target` and `Context.prototype` plain state tables (`tkv`, `tpkv`)
-- so hot reads (`ctx.host`, `ctx.fiber`, `ctx.extend`, `ctx:on`, `ctx.svc`) skip weak-table
-- state lookups, `readPlainState`/`isSpecialProperty` calls, and double `getTraceable` checks.
local rootHandler
rootHandler = {
  get = function(rctx, target, prop, ctx)
    if type(prop) ~= "string" then
      return get(target, prop, ctx)
    end
    local tkv, tpkv = rctx.tstate.kv, rctx.tp.kv
    if protoEpoch.n ~= rctx.pe or tkv == nil or tpkv == nil then
      local tstate, tp = rctx.tstate, rctx.tp
      if
        not (
          tkv
          and tpkv
          and not tstate.handler
          and tstate.pstate == tp
          and not tp.handler
          and tp.pstate == nil
          and tp.proto == nil
        )
      then
        return M.handler.get(target, prop, ctx)
      end
      rctx.pe = protoEpoch.n
    end
    -- 1. Own properties on root target (`host`, `fiber`, `events`, `reflect`, `registry`, `logger`)
    local tv = tkv[prop]
    if tv ~= nil then
      if type(tv) ~= "table" then
        return tv
      end
      local mt = getmetatable(tv)
      if mt == nil then
        if tv[SHADOW] == nil and tv[TRACKER] == nil then
          return tv
        end
        local b = byte(prop, 1)
        if b ~= nil and b <= 95 and isSpecialProperty(prop) then
          return tv
        end
        return getTraceable(ctx, tv)
      elseif classes[mt] then
        if rawget(tv, SHADOW) == nil then
          local tr = rawget(tv, TRACKER)
          if tr ~= nil then
            local b = byte(prop, 1)
            if b ~= nil and b <= 95 and isSpecialProperty(prop) then
              return tv
            end
            local byCtx = traceables[tv]
            local thit = byCtx and byCtx[ctx]
            if thit and thit[1] == tr then
              return thit[2]
            end
            return createTraceable(ctx, tv, tr)
          end
          if classLookup(mt, TRACKER, tv) == nil then
            return tv
          end
        end
        local b = byte(prop, 1)
        if b ~= nil and b <= 95 and isSpecialProperty(prop) then
          return tv
        end
        return getTraceable(ctx, tv)
      end
      local b = byte(prop, 1)
      if b ~= nil and b <= 95 and isSpecialProperty(prop) then
        return tv
      end
      return getTraceable(ctx, tv)
    end
    -- 2. Direct `Context.prototype` methods (`extend`, `isolate`, `intercept`, `close`)
    local pv = tpkv[prop]
    if pv ~= nil then
      if type(pv) ~= "table" then
        return pv
      end
      local b = byte(prop, 1)
      if b ~= nil and b <= 95 and isSpecialProperty(prop) then
        return pv
      end
      return getTraceable(ctx, pv)
    end
    -- 3. Remembered service / mixin read for this context
    local byProp = resolved[ctx]
    local hit = byProp and byProp[prop]
    if hit and rawequal(hit[1], target) then
      local kind = hit[5]
      if kind == 4 then
        -- Root-target mixin method (`ctx:on`, `ctx:emit`, `ctx:use`, `ctx:provide`, ...):
        -- survives unrelated `resolveEpoch` bumps and validates against `mixinEpoch + ctxVersion.n`,
        -- live `tkv[sourceKey]`, and live method function identity.
        if hit[2] == mixinEpoch + ctxVersion.n then
          local bound = hit[3]
          local rawSvc = hit[4]
          local origFn = hit[7]
          local srcKey = hit[10]
          if bound and rawSvc and origFn and srcKey and rawequal(tkv[srcKey], rawSvc) then
            local propKey = hit[6]
            if hit[9] then
              if rawequal(rawget(rawSvc, propKey), origFn) then
                return bound
              end
            else
              local cls = hit[8]
              local getters = cls and classHasGetters[cls]
              if
                rawget(rawSvc, propKey) == nil
                and cls
                and rawequal(rawget(cls, propKey), origFn)
                and (not getters or not getters[propKey])
              then
                return bound
              end
            end
          end
        end
      elseif hit[2] == resolveEpoch.n then
        if kind == 1 then
          local impl = hit[4]
          if impl then
            local hooks = hit[3]["internal/get"]
            if hooks == nil or #hooks == 0 then
              local val = impl.value
              if type(val) ~= "table" then
                return val
              end
              local byCtx = traceables[val]
              local thit = byCtx and byCtx[ctx]
              if thit then
                local mt = getmetatable(val)
                local tr = mt == nil and val[TRACKER] or (classes[mt] and rawget(val, TRACKER))
                if tr ~= nil and thit[1] == tr and (mt == nil and val[SHADOW] == nil or rawget(val, SHADOW) == nil) then
                  return thit[2]
                end
              end
              return getTraceable(ctx, val)
            end
          end
        elseif kind == 3 then
          local bound = hit[3]
          local rawSvc = hit[4]
          local origFn = hit[7]
          if bound and rawSvc and origFn then
            local propKey = hit[6]
            if hit[9] then
              if rawequal(rawget(rawSvc, propKey), origFn) then
                return bound
              end
            else
              local cls = hit[8]
              local getters = cls and classHasGetters[cls]
              if
                rawget(rawSvc, propKey) == nil
                and cls
                and rawequal(rawget(cls, propKey), origFn)
                and (not getters or not getters[propKey])
              then
                return bound
              end
            end
          end
        end
      end
    end
    local b = byte(prop, 1)
    if b ~= nil and b <= 95 and isSpecialProperty(prop) then
      return get(target, prop, ctx)
    end
    local epoch = resolveEpoch.n
    local tReflect = tkv.reflect
    local def = tReflect.props[prop]
    if def and def.type == "accessor" then
      local getter = def.get
      local meta = mixinGetters[getter]
      if meta then
        local receiver = getUntilHandler(ctx, S.receiver, rootHandler)
        if receiver == nil then
          receiver = tkv[S.receiver]
          if receiver == nil then
            receiver = tpkv[S.receiver]
          end
        end
        local srcKey = meta.key
        -- Direct fast path for root-target class mixins (`events`, `reflect`, `registry`) when
        -- there is no overriding receiver and no string own key on `ctx` shadows `srcKey`.
        if receiver == nil and not tReflect.props[srcKey] then
          local cs = protoState[ctx]
          local noShadowKey = cs == nil
            or cs.handler ~= nil
            or (cs.strEpoch == proto._strSkipEpoch() and cs.k1 ~= srcKey and cs.strOwnOnly ~= srcKey)
          if noShadowKey then
            local rawSvc = tkv[srcKey]
            if type(rawSvc) == "table" then
              local mt = getmetatable(rawSvc)
              if classes[mt] and rawget(rawSvc, SHADOW) == nil then
                local propKey = meta.propKey
                local ownFn = rawget(rawSvc, propKey)
                local fn, isOwn
                if type(ownFn) == "function" then
                  fn, isOwn = ownFn, true
                elseif ownFn == nil then
                  local getters = classHasGetters[mt]
                  local clsFn = (not getters or not getters[propKey]) and rawget(mt, propKey) or nil
                  if type(clsFn) == "function" then
                    fn, isOwn = clsFn, false
                  end
                end
                if fn then
                  local tr = rawget(rawSvc, TRACKER)
                  if tr ~= nil and tr.noShadow then
                    local byCtx = traceables[rawSvc]
                    local thit = byCtx and byCtx[ctx]
                    local service = (thit and thit[1] == tr) and thit[2] or createTraceable(ctx, rawSvc, tr)
                    local byKey = mixinBound[service]
                    local bhit = byKey and byKey[propKey]
                    local bound
                    if bhit and rawequal(bhit[1], fn) then
                      bound = bhit[2]
                    else
                      bound = function(_, ...)
                        return fn(service, ...)
                      end
                      if not byKey then
                        byKey = {}
                        mixinBound[service] = byKey
                      end
                      byKey[propKey] = { fn, bound }
                    end
                    storeResolved(
                      ctx,
                      prop,
                      target,
                      mixinEpoch + ctxVersion.n,
                      bound,
                      rawSvc,
                      4,
                      propKey,
                      fn,
                      mt,
                      isOwn,
                      srcKey
                    )
                    return bound
                  end
                end
              end
            end
          end
        end
        local res = getter(ctx, receiver)
        if receiver == nil and type(res) == "function" and resolveEpoch.n == epoch and not tReflect.props[srcKey] then
          local rawSvc = tkv[srcKey]
          if rawSvc == nil then
            rawSvc = ctx[srcKey]
          end
          local ss = protoState[rawSvc]
          while ss and ss.handler do
            rawSvc = ss.target
            ss = protoState[rawSvc]
          end
          if type(rawSvc) == "table" then
            local mt = getmetatable(rawSvc)
            if classes[mt] then
              local propKey = meta.propKey
              local ownFn = rawget(rawSvc, propKey)
              if type(ownFn) == "function" then
                storeResolved(ctx, prop, target, epoch, res, rawSvc, 3, propKey, ownFn, mt, true)
              elseif ownFn == nil then
                local getters = classHasGetters[mt]
                local clsFn = (not getters or not getters[propKey]) and rawget(mt, propKey) or nil
                if type(clsFn) == "function" then
                  storeResolved(ctx, prop, target, epoch, res, rawSvc, 3, propKey, clsFn, mt, false)
                end
              end
            end
          end
        end
        return res
      end
      local err = createAccessError(string.format('cannot get property "%s" without inject', prop))
      local ok, result = pcall(getter, ctx, ctx[S.receiver], err)
      if not ok then
        rethrow(result, err)
      end
      return result
    end

    local shadow = getUntilHandler(ctx, S.shadow, rootHandler)
    if shadow == nil then
      shadow = tkv[S.shadow]
      if shadow == nil then
        shadow = tpkv[S.shadow]
      end
    end
    local defSite = shadow or ctx
    if not defSite.fiber.runtime then
      return ctx.reflect:get(prop, false)
    end

    local eventsSvc = tkv.events
    local allHooks = eventsSvc and eventsSvc._hooks or ctx.events._hooks
    local hooks = allHooks["internal/get"]
    if hooks == nil or #hooks == 0 then
      local found, _, foundImpl = locateValues(defSite, prop)
      if not found then
        error(errors.Error.new(string.format('cannot get property "%s" without inject', prop)), 0)
      end
      if foundImpl then
        if resolveEpoch.n == epoch then
          storeResolved(ctx, prop, target, epoch, allHooks, foundImpl, 1)
        end
        return getTraceable(ctx, foundImpl.value)
      end
      error(errors.Error.new(string.format('cannot get required service "%s" in inactive context', prop)), 0)
    end

    local err = createAccessError(string.format('cannot get property "%s" without inject', prop))
    local ok, result = pcall(function()
      local function fallback()
        local found, _, foundImpl = locateValues(defSite, prop)
        if not found then
          error(err, 0)
        end
        if foundImpl then
          if resolveEpoch.n == epoch then
            storeResolved(ctx, prop, target, epoch, allHooks, foundImpl, 1)
          end
          return getTraceable(ctx, foundImpl.value)
        end
        err.message = string.format('cannot get required service "%s" in inactive context', prop)
        error(err, 0)
      end
      return ctx.events:waterfall("internal/get", ctx, prop, err, fallback)
    end)
    if not ok then
      rethrow(result, err)
    end
    return result
  end,
  set = function(_, target, prop, value, ctx)
    return M.handler.set(target, prop, value, ctx)
  end,
  has = function(_, target, prop)
    return M.handler.has(target, prop)
  end,
}

function M._createRootProxy(target, contextProto)
  local rctx = { tstate = protoState[target], tp = protoState[contextProto], pe = protoEpoch.n }
  return proto._proxyWithContext(target, rootHandler, rctx, true)
end

local ReflectService = class("ReflectService")
M.ReflectService = ReflectService

local function unwrapReflect(self)
  local s = protoState[self]
  if s and s.handler then
    local raw = s.target
    if getmetatable(raw) == ReflectService then
      local tc = s.context
      local ctx = tc and tc.tracker and tc.tracker.property == "ctx" and tc.useSite or self.ctx
      return raw, ctx
    end
  elseif getmetatable(self) == ReflectService then
    return self, rawget(self, "ctx")
  end
  return self, self.ctx
end

function ReflectService:constructor(ctx)
  self.ctx = ctx
  self.store = {}
  self.props = {}
  self._implCounter = 0
  rawset(self, S.tracker, { property = "ctx", noShadow = true })

  self:mixin("reflect", { "get", "set", "provide", "accessor", "mixin" })
  self:mixin("fiber", { "runtime", "effect" })
  self:mixin("registry", { "inject", "use", "plugin" })
  self:mixin("events", { "on", "once", "parallel", "emit", "serial", "bail", "waterfall" })
end

function ReflectService:get(name, strict)
  if strict == nil then
    strict = true
  end
  local raw, ctx = unwrapReflect(self)
  local key = ctx[S.isolate][name]
  local impl = key and raw.store[key]
  if not impl or (strict and impl.ctx.fiber.state ~= state().ACTIVE) then
    return nil
  end
  return getTraceable(ctx, impl.value)
end

function ReflectService:getImpl(name, strict)
  if strict == nil then
    strict = true
  end
  local raw, ctx = unwrapReflect(self)
  local key = ctx[S.isolate][name]
  local impl = key and raw.store[key]
  if not impl then
    return nil
  end
  if strict and impl.ctx.fiber.state ~= state().ACTIVE then
    return nil
  end
  return impl
end

function ReflectService:set(name, value)
  local raw, ctx = unwrapReflect(self)
  local key = ctx[S.isolate][name]
  local impl = key and raw.store[key]
  if not impl then
    error(errors.Error.new(string.format('cannot set property "%s" without provide', name)), 0)
  end
  if impl.ctx.fiber ~= ctx.fiber then
    error(errors.Error.new(string.format('cannot set property "%s" in multiple fibers', name)), 0)
  end
  emitInternalProvide(ctx, name, value)
  impl.value = value
  return true
end

local function label(fmt, name)
  if type(name) == "string" then
    return string.format(fmt, string.format("%q", name))
  end
  return string.format(fmt, tostring(name))
end

function ReflectService:provide(name, value, check)
  local this = self
  local ctx = self.ctx
  local Iter = require("cordis.fiber")
  local scope = future.capture(ctx.host)
  return ctx.fiber:effect(
    Iter.iter(function(yield)
      emitInternalProvide(ctx, name, value)
      local props = this.props
      if not props[name] then
        props[name] = { type = "service" }
        if mixinTargets[name] then
          mixinEpoch = mixinEpoch + 1
        end
      elseif props[name].type ~= "service" then
        error(errors.Error.new(string.format('property "%s" is already declared as %s', name, props[name].type)), 0)
      end
      props[name] = { type = "service" }
      if mixinTargets[name] then
        mixinEpoch = mixinEpoch + 1
      end

      local rootIsolate = ctx[S.root][S.isolate]
      if rootIsolate[name] == nil then
        rootIsolate[name] = symbols.symbol(name)
      end
      local key = ctx[S.isolate][name]
      local impl = {
        uid = this._implCounter + 1,
        name = name,
        value = value,
        ctx = ctx,
        check = check,
        scope = scope,
        isolate = key,
        dependents = Set.new(),
      }
      this._implCounter = impl.uid
      if this.store[key] then
        error(
          errors.Error.new(
            string.format('service "%s" has been registered at <%s>', name, this.store[key].ctx.fiber.name)
          ),
          0
        )
      end
      this.store[key] = impl
      ctx.fiber.provisions:add(impl)
      resolveEpoch.bump() -- `locate` reads provisions
      yield(future.internal(function()
        if this.store[impl.isolate] == impl then
          this.store[impl.isolate] = nil
        end
        local notifyOk, notifyErr = pcall(function()
          ctx:isolate(name, impl.isolate).reflect:notify({ name })
        end)
        -- Retired bindings cannot gain new dependents. Wait only for their
        -- cleanup, even if a fiber has left the registry or starts a new load.
        -- Yield even without direct dependents so child cleanup can still
        -- access the inherited service before its provision is removed.
        local host = ctx.host
        local released, release = Future.withResolvers(host)
        impl.release = release
        if impl.dependents:size() == 0 then
          release()
        end
        return released
          :_next(function()
            -- one hop, so that child cleanup started meanwhile still sees the service
            return future.hop(host)
          end)
          :_next(function()
            impl.release = nil
            -- ensure self access before dependencies cleanup
            ctx.fiber.provisions:delete(impl)
            resolveEpoch.bump() -- invalidate cached lookups
            if not notifyOk then
              error(notifyErr, 0)
            end
          end)
      end))
      if ctx.fiber.state == state().ACTIVE then
        this:notify({ name })
      end
    end),
    label("ctx.provide(%s)", name)
  )
end

function ReflectService:notify(names, filter)
  -- no names, nothing to notify
  local nNames = #names
  if nNames == 0 then
    return
  end
  local raw, selfCtx = unwrapReflect(self)
  local defaultFilter = filter == nil
  local thisIso, singleName, singleKey
  if defaultFilter then
    thisIso = selfCtx[S.isolate]
    if nNames == 1 then
      singleName = names[1]
      singleKey = thisIso[singleName]
    end
    filter = function(ctx, name)
      return ctx[S.isolate][name] == (name == singleName and singleKey or thisIso[name])
    end
  end
  local errs
  local function record(ok, err)
    if not ok then
      errs = errs or {}
      errs[#errs + 1] = err
    end
  end
  -- the walk (in upstream order) only when some fiber injects one of the
  -- names, and only those fibers are checked (cordis/inject_index.lua)
  if injectIndex.any(names) then
    for _, runtime in ipairs(selfCtx.registry:values()) do
      if injectIndex.anyIn(runtime, names) then
        for _, fiber in ipairs(runtime.fibers:values()) do
          if injectIndex.injects(fiber, names) then
            local hasUpdate = false
            record(pcall(function()
              for _, name in ipairs(names) do
                if has(fiber.inject, name) and filter(fiber.ctx, name) then
                  hasUpdate = true
                  fiber:_checkImpl(name)
                end
              end
              if hasUpdate then
                fiber:_refresh()
              end
            end))
          end
        end
      end
    end
  end
  local eventsSvc = selfCtx.events
  for _, name in ipairs(names) do
    local thisArg = proto.create(selfCtx)
    thisArg[S.filter] = function(_, target)
      return filter(target, name)
    end
    local key = defaultFilter and (name == singleName and singleKey or thisIso[name]) or selfCtx[S.isolate][name]
    local impl = key and raw.store[key]
    record(pcall(eventsSvc.emit, eventsSvc, thisArg, "internal/service", name, impl and impl.value))
  end
  if errs then
    error(#errs == 1 and errs[1] or errors.AggregateError.new(errs, "one or more observers failed"), 0)
  end
end

function ReflectService:accessor(name, options)
  local this = self
  return self.ctx.fiber:effect(function()
    emitInternalProvide(this.ctx, name, options)
    if this.props[name] then
      error(errors.Error.new(string.format('property "%s" is already declared as %s', name, this.props[name].type)), 0)
    end
    this.props[name] = { type = "accessor", get = options.get, set = options.set }
    mixinEpoch = mixinEpoch + 1
    resolveEpoch.bump()
    return future.internal(function()
      this.props[name] = nil
      mixinEpoch = mixinEpoch + 1
      resolveEpoch.bump()
    end)
  end, label("ctx.accessor(%s)", name))
end

function ReflectService:mixin(source, mixins)
  local this = self
  local Iter = require("cordis.fiber")
  return self.ctx.fiber:effect(
    Iter.iter(function(yield)
      local entries = {}
      if mixins[1] ~= nil then
        for i, key in ipairs(mixins) do
          entries[i] = { key, key }
        end
      elseif proto.is(mixins) then
        entries = proto.entries(mixins)
      else
        for k, v in pairs(mixins) do
          entries[#entries + 1] = { k, v }
        end
        table.sort(entries, function(a, b)
          return tostring(a[1]) < tostring(b[1])
        end)
      end
      -- `ctx[source]` as upstream (reflect.ts:286-289): a non-string,
      -- non-symbol source is converted like a JS property key, so an object
      -- source reads the property "[object Object]"
      local key = source
      if type(source) ~= "string" and not symbols.isSymbol(source) then
        key = "[object Object]"
      end
      mixinTargets[key] = true
      local function getTarget(ctx)
        return ctx[key]
      end
      for _, entry in ipairs(entries) do
        local propKey, alias = entry[1], entry[2]
        local getter = function(ctx, receiver)
          local service = getTarget(ctx)
          if service == nil then
            return service
          end
          local mixin = receiver and trace.withProps(receiver, service) or service
          local value = get(service, propKey, mixin)
          if not proto.callable(value) or getmetatable(value) == Symbol then
            return value
          end
          -- bound method: the receiver passed by the caller is replaced; reuse per
          -- (service, propKey) when there is no overriding receiver and the method is unchanged
          if not receiver and type(service) == "table" then
            local byKey = mixinBound[service]
            local hit = byKey and byKey[propKey]
            if hit and rawequal(hit[1], value) then
              return hit[2]
            end
            local bound = function(_, ...)
              return value(service, ...)
            end
            if not byKey then
              byKey = {}
              mixinBound[service] = byKey
            end
            byKey[propKey] = { value, bound }
            return bound
          end
          return function(_, ...)
            return value(mixin or service, ...)
          end
        end
        mixinGetters[getter] = { key = key, propKey = propKey }
        yield(this:accessor(alias, {
          get = getter,
          set = function(ctx, value, receiver)
            local service = getTarget(ctx)
            local mixin = receiver and trace.withProps(receiver, service) or service
            return set(service, propKey, value, mixin)
          end,
        }))
      end
    end),
    label("ctx.mixin(%s)", source)
  )
end

function ReflectService:trace(value)
  return getTraceable(self.ctx, value)
end

local function traceArgs(ctx, n, a, b, c, ...)
  if n == 0 then
    return
  elseif n == 1 then
    return type(a) == "table" and getTraceable(ctx, a) or a
  elseif n == 2 then
    return type(a) == "table" and getTraceable(ctx, a) or a, type(b) == "table" and getTraceable(ctx, b) or b
  elseif n == 3 then
    return type(a) == "table" and getTraceable(ctx, a) or a,
      type(b) == "table" and getTraceable(ctx, b) or b,
      type(c) == "table" and getTraceable(ctx, c) or c
  end
  local args = { a, b, c, ... }
  for i = 1, n do
    if type(args[i]) == "table" then
      args[i] = getTraceable(ctx, args[i])
    end
  end
  return table.unpack(args, 1, n)
end

local BIND_HANDLER = {
  apply = function(ctx, target, _, ...)
    if type(target) == "function" then
      return target(traceArgs(ctx, select("#", ...), ...))
    end
    return proto.apply(target, target, traceArgs(ctx, select("#", ...), ...))
  end,
}

function ReflectService:bind(callback)
  local _, ctx = unwrapReflect(self)
  if future.AsyncFunction.is(callback) then
    local fn = callback.fn
    return future.async(function(...)
      return fn(traceArgs(ctx, select("#", ...), ...))
    end)
  end
  return proto._proxyWithContext(callback, BIND_HANDLER, ctx)
end

return M
