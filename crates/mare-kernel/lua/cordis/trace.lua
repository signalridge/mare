-- Ported from cordis packages/core/src/utils.ts (`:86-298`) (MIT, (c) Shigma)
--
-- Traceable services: a service value carrying a tracker is wrapped in a proxy
-- that remembers the context it was reached from.
local proto = require("cordis.proto")
local class = require("cordis.class")
local symbols = require("cordis.symbols")
local S = symbols.symbols
local errors = require("cordis.error")

local M = {}

local get, set, has, defineOwn = proto.get, proto.set, proto.has, proto.defineOwn
local proxyWithContext = proto._proxyWithContext
local type = type
local rawget = rawget
local rawequal = rawequal
local getmetatable = getmetatable
local setmetatable = setmetatable

local WEAK_KEYS = { __mode = "k" }

local Symbol = symbols._Symbol
local NULL = symbols.null

local function isObject(value)
  local t = type(value)
  return (t == "table" and getmetatable(value) ~= Symbol and value ~= NULL) or t == "function"
end
M.isObject = isObject

-- a component is a constructor when it is a cordis.class
function M.isConstructor(fn)
  return class.isClass(fn)
end

local classLookup = class.lookup
local classes = class._classes
local classHasGetters = class._hasGetters
local classSuper = class._super
local SHADOW, TRACKER = S.shadow, S.tracker
local ORIGINAL, CALLER = S.original, S.caller

function M.getTraceable(ctx, value)
  if type(value) ~= "table" then
    return value
  end
  -- a plain table or a class instance keeps its own fields in the table itself, where
  -- `proto.hasOwn` and `proto.get` read them (a class instance's inherited fields through its
  -- class chain), so they are read directly; proto objects, symbols and null take the general path
  local mt = getmetatable(value)
  if mt == nil then
    -- no metatable: indexing reads the table's own fields
    if value[SHADOW] ~= nil then
      return proto.getPrototypeOf(value)
    end
    local tracker = value[TRACKER]
    if not tracker then
      return value
    end
    return M.createTraceable(ctx, value, tracker)
  end
  -- class instances have mt in `classes` (which excludes OBJ, symbols and null)
  if classes[mt] then
    if rawget(value, SHADOW) ~= nil then
      return proto.getPrototypeOf(value)
    end
    local tracker = rawget(value, TRACKER)
    if tracker == nil then
      tracker = classLookup(mt, TRACKER, value)
    end
    if not tracker then
      return value
    end
    return M.createTraceable(ctx, value, tracker)
  end
  if not isObject(value) then
    return value
  end
  if proto.hasOwn(value, SHADOW) then
    return proto.getPrototypeOf(value)
  end
  local tracker = get(value, TRACKER, value)
  if not tracker then
    return value
  end
  return M.createTraceable(ctx, value, tracker)
end

local WITH_PROPS_HANDLER = {
  get = function(props, t, prop, receiver)
    if has(props, prop) and prop ~= "constructor" then
      return get(props, prop, receiver)
    end
    return get(t, prop, receiver)
  end,
  set = function(props, t, prop, value, receiver)
    if has(props, prop) and prop ~= "constructor" then
      return set(props, prop, value, receiver)
    end
    return set(t, prop, value, receiver)
  end,
}

local function withProps(target, props)
  if not props then
    return target
  end
  return proxyWithContext(target, WITH_PROPS_HANDLER, props)
end
M.withProps = withProps

local WITH_PROP_HANDLER = {
  get = function(box, t, prop, receiver)
    if box[2] ~= nil and prop == box[1] and prop ~= "constructor" then
      return box[2]
    end
    return get(t, prop, receiver)
  end,
  set = function(box, t, prop, value, receiver)
    if box[2] ~= nil and prop == box[1] and prop ~= "constructor" then
      return defineOwn(receiver, prop, value)
    end
    return set(t, prop, value, receiver)
  end,
}

local function withProp(target, prop, value)
  return proxyWithContext(target, WITH_PROP_HANDLER, { prop, value })
end
M.withProp = withProp

-- the plain (non-getter) value of `prop` along a class chain: per class its getter first, then its
-- member, then the parent class
local function classDataValue(cls, prop)
  local c = cls
  while c do
    local getters = classHasGetters[c]
    if getters and getters[prop] then
      return nil, false
    end
    local cv = rawget(c, prop)
    if cv ~= nil then
      return cv, true
    end
    c = classSuper[c]
  end
  return nil, false
end

-- own or inherited plain (non-getter) value of `prop` on `target`
local function getDataValue(target, prop)
  -- a plain table or class instance (the receiver of a service method) is read in place
  if type(target) == "table" then
    local mt = getmetatable(target)
    if mt == nil then
      local v = target[prop]
      return v, v ~= nil
    end
    if classes[mt] then
      local v = rawget(target, prop)
      if v ~= nil then
        return v, true
      end
      return classDataValue(mt, prop)
    end
  end
  local cur = target
  while cur ~= nil do
    if proto.is(cur) then
      if proto.isProxy(cur) then
        -- a proxy without descriptor traps exposes its target's descriptors
        cur = proto.proxyTarget(cur)
      elseif proto.hasOwn(cur, prop) then
        return proto.getOwn(cur, prop), true
      else
        cur = proto.getPrototypeOf(cur)
      end
    elseif type(cur) == "table" then
      local v = rawget(cur, prop)
      if v ~= nil then
        return v, true
      end
      local cls = class.classOf(cur)
      if not cls then
        return nil, false
      end
      return classDataValue(cls, prop)
    else
      return nil, false
    end
  end
  return nil, false
end

local function createShadow(useSite, target, property, receiver)
  if not property then
    return receiver
  end
  local value = getDataValue(target, property)
  if not value then
    return receiver
  end
  local defSite = get(value, S.shadow, value) or value
  return withProp(receiver, property, useSite:extend({ [S.shadow] = defSite }))
end

-- the result of a traced call: upstream traces its single result; a Lua method
-- may return several values, the first is traced and the rest pass through
--
local function traceResults(ctx, ...)
  if select("#", ...) == 0 then
    return
  end
  local first = ...
  if type(first) ~= "table" then
    return ...
  end
  return M.getTraceable(ctx, first), select(2, ...)
end

-- a traced method: calling it with the outer proxy as receiver substitutes
-- the shadow (the receiver is the first argument). A plain Lua
-- function stays a function (upstream's Proxy of a function is still
-- `typeof === "function"`; consumers check methods with `type(m) ==
-- "function"`); a callable table keeps its other properties through a proxy
local function createShadowMethod(ctx, value, outer, shadow)
  if type(value) == "function" then
    return function(thisArg, ...)
      if rawequal(thisArg, outer) then
        thisArg = shadow
      end
      return traceResults(ctx, value(thisArg, ...))
    end
  end
  return proto.proxy(value, {
    apply = function(target, _, thisArg, ...)
      if rawequal(thisArg, outer) then
        thisArg = shadow
      end
      return traceResults(ctx, proto.apply(target, target, thisArg, ...))
    end,
  })
end

local function applyTraceable(receiver, value, ...)
  local invoke = get(value, S.invoke, value)
  if not invoke then
    return proto.apply(value, value, ...)
  end
  return invoke(receiver, ...)
end

-- `ctx` may be a shadow, i.e. a pair of:
-- - def site: where the accessing code was defined, governs service resolution
-- - use site: where the service is consumed, governs intercept, isolate and effects
-- one traceable per (value, ctx, tracker). Upstream builds a new proxy on
-- every read; the proxy holds no state of its own beyond these three, so a
-- cached one behaves the same. Weak keys on both levels (ephemerons): the cache
-- keeps neither the value nor the context alive.
local traceables = setmetatable({}, WEAK_KEYS)
M._traceables = traceables

local buildTraceable

function M.createTraceable(ctx, value, tracker)
  local byCtx = traceables[value]
  local hit = byCtx and byCtx[ctx]
  if hit and hit[1] == tracker then
    return hit[2]
  end
  local proxy = buildTraceable(ctx, value, tracker)
  if not byCtx then
    byCtx = setmetatable({}, WEAK_KEYS)
    traceables[value] = byCtx
  end
  byCtx[ctx] = { tracker, proxy }
  return proxy
end

local function tcShadowOf(tc, target, receiver)
  local prop = tc.tracker.property
  if type(receiver) ~= "table" or not prop then
    return createShadow(tc.useSite, target, prop, receiver)
  end
  local site = rawget(target, prop)
  if site == nil then
    site = getDataValue(target, prop)
  end
  if rawequal(receiver, tc.proxy) then
    local selfShadow = tc.selfShadow
    if selfShadow and rawequal(selfShadow[1], site) then
      return selfShadow[2]
    end
    local shadow = createShadow(tc.useSite, target, prop, receiver)
    tc.selfShadow = { site, shadow }
    return shadow
  end
  local shadows = tc.shadows
  local hit = shadows and shadows[receiver]
  if hit and rawequal(hit[1], site) then
    return hit[2]
  end
  local shadow = createShadow(tc.useSite, target, prop, receiver)
  if not shadows then
    shadows = setmetatable({}, WEAK_KEYS)
    tc.shadows = shadows
  end
  shadows[receiver] = { site, shadow }
  return shadow
end

local function tcAssociated(tc, prop)
  local assocPrefix = tc.assocPrefix
  if not assocPrefix then
    return false
  end
  local assocProps = tc.assocProps
  if not assocProps then
    assocProps = tc.useSite.reflect.props
    tc.assocProps = assocProps
  end
  if prop ~= tc.lastProp then
    tc.lastProp, tc.lastKey = prop, assocPrefix .. prop
  end
  return assocProps[tc.lastKey] ~= nil
end

local function tcSiteOf(tc, target)
  local key = tc.tracker.property
  if not key then
    return nil
  end
  local site = rawget(target, key)
  if site == nil then
    site = getDataValue(target, key)
  end
  return site
end

local function tcNewShadowMethod(tc, target, receiver, prop, innerValue, site, byName)
  local method = createShadowMethod(tc.useSite, innerValue, receiver, tcShadowOf(tc, target, receiver))
  if not byName then
    local methods = tc.methods
    if not methods then
      methods = setmetatable({}, WEAK_KEYS)
      tc.methods = methods
    end
    byName = {}
    methods[receiver] = byName
  end
  byName[prop] = { innerValue, site, method }
  return method
end

local function tcShadowMethod(tc, target, receiver, prop, innerValue)
  local site = tcSiteOf(tc, target)
  local methods = tc.methods
  local byName = methods and methods[receiver]
  local hit = byName and byName[prop]
  if hit and rawequal(hit[1], innerValue) and rawequal(hit[2], site) then
    return hit[3]
  end
  return tcNewShadowMethod(tc, target, receiver, prop, innerValue, site, byName)
end

local TRACEABLE_HANDLER = {
  get = function(tc, target, prop, receiver)
    if type(prop) == "string" then
      local tracker = tc.tracker
      if prop == tracker.property then
        return tc.useSite
      end
      -- `ctx` rather than `useSite`, so that `ctx.foo.bar` and `ctx['foo.bar']`
      -- are governed by the same def site
      if tc.assocPrefix and tcAssociated(tc, prop) then
        local ctx = tc.ctx
        return get(ctx, tc.lastKey, withProp(ctx, S.receiver, receiver))
      end
      -- Cache-first fast path when the receiver is the traceable proxy itself and we already
      -- cached the shadow method for `prop` (`ownerKind`: `true` for own field, class `mt` for
      -- class method defined directly on `target`'s class). Validates live target/class/def-site
      -- in 2-3 rawgets, including `cordis.Service` subclasses.
      local isSelf = rawequal(receiver, tc.proxy)
      if isSelf then
        local sm = tc.selfMethods
        local hit = sm and sm[prop]
        if hit then
          local ownerKind = hit[4]
          if ownerKind == true then
            local siteKey = hit[5]
            if
              rawequal(rawget(target, prop), hit[1])
              and (siteKey == nil or rawequal(rawget(target, siteKey), hit[2]))
            then
              return hit[3]
            end
          elseif ownerKind ~= nil then
            local getters = classHasGetters[ownerKind]
            local siteKey = hit[5]
            if
              rawget(target, prop) == nil
              and (not getters or not getters[prop])
              and rawequal(rawget(ownerKind, prop), hit[1])
              and (siteKey == nil or rawequal(rawget(target, siteKey), hit[2]))
            then
              return hit[3]
            end
          end
        end
      end
      -- Inline fast path for method reads on plain-table and class targets
      -- when the receiver is the traceable proxy itself (the common `ctx.svc:method(...)` path).
      local v, isData, ownerKind
      if type(target) == "table" then
        local mt = getmetatable(target)
        if mt == nil then
          v = target[prop]
          isData = v ~= nil
          ownerKind = true
        elseif classes[mt] then
          v = rawget(target, prop)
          if v ~= nil then
            isData = true
            ownerKind = true
          else
            local getters = classHasGetters[mt]
            if not getters or not getters[prop] then
              local mv = rawget(mt, prop)
              if mv ~= nil then
                v, isData, ownerKind = mv, true, mt
              elseif classSuper[mt] then
                v, isData = classDataValue(classSuper[mt], prop)
              end
            else
              v, isData = nil, false
            end
          end
        else
          v, isData = getDataValue(target, prop)
        end
      else
        v, isData = getDataValue(target, prop)
      end
      if isData and type(v) == "function" and not tracker.noShadow and type(receiver) == "table" then
        local siteKey = tracker.property
        local site, rawSite
        if siteKey then
          site = rawget(target, siteKey)
          if site ~= nil then
            rawSite = true
          else
            site = getDataValue(target, siteKey)
          end
        end
        if isSelf then
          local sm = tc.selfMethods
          local hit = sm and sm[prop]
          if hit and hit[1] == v and rawequal(hit[2], site) then
            return hit[3]
          end
          local method = createShadowMethod(tc.useSite, v, receiver, tcShadowOf(tc, target, receiver))
          if not sm then
            sm = {}
            tc.selfMethods = sm
          end
          sm[prop] = { v, site, method, (siteKey == nil or rawSite) and ownerKind or nil, siteKey }
          return method
        end
        local methods = tc.methods
        local byName = methods and methods[receiver]
        local hit = byName and byName[prop]
        if hit and hit[1] == v and rawequal(hit[2], site) then
          return hit[3]
        end
        return tcNewShadowMethod(tc, target, receiver, prop, v, site, byName)
      end
      local shadow, innerValue
      if isData then
        innerValue = v
      else
        shadow = tcShadowOf(tc, target, receiver)
        innerValue = get(target, prop, shadow)
      end
      local ivType = type(innerValue)
      if ivType ~= "table" then
        if ivType == "function" and not tracker.noShadow then
          if isData and type(receiver) == "table" then
            return tcShadowMethod(tc, target, receiver, prop, innerValue)
          end
          shadow = shadow or tcShadowOf(tc, target, receiver)
          return createShadowMethod(tc.useSite, innerValue, receiver, shadow)
        end
        return innerValue
      end
      local innerTracker
      local mt = getmetatable(innerValue)
      if mt == nil then
        innerTracker = innerValue[TRACKER]
      elseif classes[mt] then
        innerTracker = rawget(innerValue, TRACKER)
        if innerTracker == nil then
          innerTracker = classLookup(mt, TRACKER, innerValue)
        end
      elseif mt ~= Symbol and innerValue ~= NULL then
        innerTracker = get(innerValue, TRACKER, innerValue)
      end
      if innerTracker then
        return M.createTraceable(tc.useSite, innerValue, innerTracker)
      elseif not tracker.noShadow and proto.callable(innerValue) and mt ~= Symbol then
        if isData and type(receiver) == "table" then
          return tcShadowMethod(tc, target, receiver, prop, innerValue)
        end
        shadow = shadow or tcShadowOf(tc, target, receiver)
        return createShadowMethod(tc.useSite, innerValue, receiver, shadow)
      else
        return innerValue
      end
    end
    if prop == ORIGINAL then
      return target
    end
    if prop == CALLER then
      return tc.defSite
    end
    if prop == tc.tracker.property then
      return tc.useSite
    end
    return get(target, prop, receiver)
  end,
  set = function(tc, target, prop, v, receiver)
    if prop == ORIGINAL then
      return false
    end
    if prop == CALLER then
      return false
    end
    if prop == tc.tracker.property then
      return false
    end
    if symbols.isSymbol(prop) or type(prop) ~= "string" then
      return set(target, prop, v, receiver)
    end
    if tc.assocPrefix and tcAssociated(tc, prop) then
      local ctx = tc.ctx
      return set(ctx, tc.lastKey, v, withProp(ctx, S.receiver, receiver))
    end
    local shadow = tcShadowOf(tc, target, receiver)
    return set(target, prop, v, shadow)
  end,
  apply = function(tc, target, _, ...)
    local proxy = tc.proxy
    local receiver = tc.tracker.noShadow and proxy or tcShadowOf(tc, target, proxy)
    return applyTraceable(receiver, target, ...)
  end,
}

function buildTraceable(ctx, value, tracker)
  local shadowSite = proto._getWatched(ctx, SHADOW)
  local defSite = shadowSite or ctx
  local useSite = shadowSite and proto.getPrototypeOf(ctx) or ctx
  local assoc = tracker.associate
  local tc = {
    ctx = ctx,
    defSite = defSite,
    useSite = useSite,
    tracker = tracker,
    assocPrefix = assoc and (assoc .. ".") or false,
  }
  local proxy = proxyWithContext(value, TRACEABLE_HANDLER, tc)
  tc.proxy = proxy
  return proxy
end

-- composeError / buildOuterStack (`:236-298`): best-effort long tracebacks

local function emptyOuterStack()
  return {}
end

function M.buildOuterStack(offset)
  local dbg = rawget(_G, "debug")
  if type(dbg) ~= "table" or type(dbg.traceback) ~= "function" then
    return emptyOuterStack
  end
  local tb = dbg.traceback("", 3 + (offset or 0))
  if tb == "" then
    return emptyOuterStack
  end
  return function()
    local lines = {}
    for line in tb:gmatch("[^\n]+") do
      if not line:match("^stack traceback:") and line ~= "" then
        lines[#lines + 1] = line
      end
    end
    return lines
  end
end

local function annotate(reason, getOuterStack)
  if type(reason) == "table" and not symbols.isSymbol(reason) then
    local cls = class.classOf(reason)
    local message = cls and rawget(reason, "message") or (not getmetatable(reason) and reason.message)
    if message ~= nil and rawget(reason, "outerStack") == nil then
      rawset(reason, "outerStack", getOuterStack())
    end
  end
  return reason
end

-- `composeError(callback, getOuterStack)`: run `callback(info)`; errors (and
-- rejections of a returned Future) carry the registration stack in
-- `outerStack`. String errors stay strings: wrapping them would change what
-- callers match on, so only error tables are annotated.
function M.composeError(callback, getOuterStack)
  getOuterStack = getOuterStack or M.buildOuterStack()
  local info = { offset = 1 }
  local ok, result = pcall(callback, info)
  if not ok then
    if type(result) == "table" then
      annotate(result, getOuterStack)
    end
    error(result, 0)
  end
  local future = require("cordis.future")
  if future.isThenable(result) then
    if not future.isFuture(result) then
      result = getmetatable(result).__future(result)
    end
    return result:_catch(function(reason)
      if type(reason) == "table" then
        annotate(reason, getOuterStack)
      end
      error(reason, 0)
    end)
  end
  return result
end

return M
