-- JS-like objects: prototype chains, insertion-ordered own
-- keys, and proxies with get / set / has / apply traps.
--
-- An object is an empty Lua table whose state lives in a hidden side table, so
-- that every read and write passes through the metamethods. Plain Lua tables
-- and `cordis.class` instances take part in the same operations (`get`, `set`,
-- `has`, ...) with their natural Lua semantics; class getters / setters receive
-- the receiver, like JS accessors.
local class = require("cordis.class")
local Map = require("cordis.ordered").Map
-- every write to an existing object bumps the resolution epoch (cordis/resolve_epoch.lua)
local epoch = require("cordis.resolve_epoch")

local type = type
local next = next
local pairs = pairs
local rawget = rawget
local rawset = rawset
local getmetatable = getmetatable
local setmetatable = setmetatable
local classes = class._classes
local classLookup = class.lookup
local classDefines = class.defines
local classFindSetter = class.findSetter

local M = {}

local state = setmetatable({}, { __mode = "k" })
M._state = state

-- Bumped whenever a string own key is mutated on a plain object that is already
-- used as a prototype AND sits under a proxy (`underProxy`), or when
-- `setPrototypeOf` rewires an `underProxy` chain. Allows plain objects with no
-- string own keys (e.g. `ctx:extend({ [S.scope] = ... })`) to jump directly to
-- their ancestor proxy state on string reads without unrelated plain-object
-- writes (such as `root[S.isolate][name] = sym`) invalidating the cache.
local strSkipEpoch = 1

-- Optional watched-key bitmask (used for rare symbol keys like `S.shadow` and
-- `S.receiver`) so lookups can prove in O(1) that no plain prototype before the
-- ancestor proxy defines the key.
local watchedKeys = {}
local watchEpoch = 1

-- Bumped only when `setPrototypeOf` rewires any prototype link, letting
-- `reflect._createRootProxy` verify `target -> ContextProto -> nil` in one check.
local protoEpoch = { n = 1 }
M._protoEpoch = protoEpoch

-- Monotonic version bumped whenever an `underProxy` prototype string key, watched key,
-- or prototype link changes (0 bumps during normal startup). Lets root mixin caches
-- survive unrelated `resolveEpoch` bumps from `fiber._store` / `provide` writes.
local ctxVersion = { n = 1 }
M._ctxVersion = ctxVersion

function M._strSkipEpoch()
  return strSkipEpoch
end

function M._watchKey(key, bit)
  watchedKeys[key] = bit
  watchEpoch = watchEpoch + 1
  ctxVersion.n = ctxVersion.n + 1
end

local function inheritChainMeta(ps)
  if not ps then
    return nil, nil, nil, nil, 0, watchEpoch
  end
  ps.isProto = true
  if ps.handler then
    return ps, strSkipEpoch, false, true, 0, watchEpoch
  end
  local wm, we
  if ps.wepoch == watchEpoch then
    wm, we = ps.wmask, watchEpoch
  end
  if ps.underProxy then
    if ps.strEpoch == strSkipEpoch then
      return ps.strState, strSkipEpoch, ps.strOwnOnly, true, wm, we
    end
    return nil, nil, nil, true, wm, we
  end
  return nil, nil, nil, nil, wm, we
end

local OBJ = { __name = "cordis.Object" }
local PROXY_OBJ = { __name = "cordis.Object" }
M.OBJ = OBJ

-- special key: a function stored under it formats `tostring(obj)`
M.inspect = require("cordis.symbols").symbolFor("cordis.inspect")

local function isObject(x)
  return state[x] ~= nil
end
M.is = isObject

-- let `Class.is(x)` see through proxies and derived objects
class.unwrap = function(x)
  local s = state[x]
  while s do
    if s.handler then
      x = s.target
    else
      x = s.proto
    end
    s = state[x]
  end
  return x
end

local function plainHas(s, k)
  if k == nil then
    return false
  end
  local k1 = s.k1
  if k1 ~= nil and k1 == k then
    return true
  end
  local kv = s.kv
  return kv ~= nil and kv[k] ~= nil
end

local function plainGet(s, k)
  if k == nil then
    return nil
  end
  local k1 = s.k1
  if k1 ~= nil and k1 == k then
    return s.v1
  end
  local kv = s.kv
  if kv then
    return kv[k]
  end
  return nil
end

local function plainDelete(s, k)
  if k == nil then
    return false
  end
  local kv = s.kv
  if kv then
    if kv[k] == nil then
      return false
    end
    kv[k] = nil
    if type(k) == "string" and s.underProxy then
      ctxVersion.n = ctxVersion.n + 1
      if s.isProto then
        strSkipEpoch = strSkipEpoch + 1
      end
    end
    if watchedKeys[k] then
      s.wepoch = nil
      ctxVersion.n = ctxVersion.n + 1
      if s.isProto then
        watchEpoch = watchEpoch + 1
      end
    end
    return s.own:delete(k)
  end
  local k1 = s.k1
  if k1 ~= nil and k1 == k then
    s.k1, s.v1 = nil, nil
    if type(k) == "string" and s.underProxy then
      ctxVersion.n = ctxVersion.n + 1
      if s.isProto then
        strSkipEpoch = strSkipEpoch + 1
      end
    end
    if watchedKeys[k] then
      s.wepoch = nil
      ctxVersion.n = ctxVersion.n + 1
      if s.isProto then
        watchEpoch = watchEpoch + 1
      end
    end
    return true
  end
  return false
end

local function plainSet(s, k, v)
  if v == nil then
    plainDelete(s, k)
    return
  end
  assert(k ~= nil, "Map key cannot be nil")
  if type(k) == "string" then
    s.hasStr = true
    s.strState, s.strEpoch, s.strOwnOnly = nil, nil, nil
    if s.underProxy then
      ctxVersion.n = ctxVersion.n + 1
      if s.isProto then
        strSkipEpoch = strSkipEpoch + 1
      end
    end
  else
    local wb = watchedKeys[k]
    if wb then
      ctxVersion.n = ctxVersion.n + 1
      if s.wepoch == watchEpoch then
        s.wmask = s.wmask | wb
      end
      if s.isProto then
        watchEpoch = watchEpoch + 1
      end
    end
  end
  local kv = s.kv
  if kv then
    kv[k] = v
    s.own:set(k, v)
    return
  end
  local k1 = s.k1
  if k1 == nil then
    s.k1, s.v1 = k, v
  elseif k1 == k then
    s.v1 = v
  else
    local own = Map.new()
    own:set(k1, s.v1)
    own:set(k, v)
    s.kv = { [k1] = s.v1, [k] = v }
    s.own = own
    s.k1, s.v1 = nil, nil
  end
end

local get, set, has, defineOwn

function get(o, k, receiver)
  local s = state[o]
  if s and not s.handler and type(k) == "string" and s.strEpoch == strSkipEpoch then
    if k == s.k1 then
      return s.v1
    end
    if k ~= s.strOwnOnly then
      s = s.strState
    end
  end
  -- the plain prototype chain is walked in a loop via direct `pstate` links
  -- without weak-table lookups or recursive calls per level
  while s and not s.handler do
    local k1 = s.k1
    if k1 ~= nil and k1 == k then
      return s.v1
    end
    local kv = s.kv
    if kv then
      local v = kv[k]
      if v ~= nil then
        return v
      end
    end
    local ps = s.pstate
    if ps then
      s = ps
    else
      o = s.proto
      if o == nil then
        return nil
      end
      s = nil
      break
    end
  end
  if s then
    local trap = s.get or s.handler.get
    if trap then
      local ctx = s.context
      if ctx ~= nil then
        return trap(ctx, s.target, k, receiver)
      end
      return trap(s.target, k, receiver)
    end
    return get(s.target, k, receiver)
  end
  local t = type(o)
  if t == "table" then
    local mt = getmetatable(o)
    if classes[mt] then
      local v = rawget(o, k)
      if v ~= nil then
        return v
      end
      return classLookup(mt, k, receiver)
    end
    return o[k]
  elseif t == "function" then
    return nil
  elseif t == "string" then
    return string[k]
  end
  return nil
end

-- internal: read `k` along `o`'s prototype/proxy chain, returning `nil` if the walk
-- reaches a proxy with `stopHandler` (avoids recursive re-entry into `reflect.handler.get`)
function M._getUntilHandler(o, k, stopHandler)
  local receiver = o
  local s = state[o]
  local wb = watchedKeys[k]
  if wb and s and not s.handler and s.wepoch == watchEpoch and (s.wmask & wb) == 0 then
    local ss = s.strEpoch == strSkipEpoch and s.strState or nil
    if ss and ss.handler == stopHandler then
      return nil
    end
  end
  while s do
    local handler = s.handler
    if handler then
      if handler == stopHandler then
        return nil
      end
      local trap = s.get or handler.get
      if trap then
        local ctx = s.context
        if ctx ~= nil then
          return trap(ctx, s.target, k, receiver)
        end
        return trap(s.target, k, receiver)
      end
      o = s.target
      s = state[o]
    else
      local k1 = s.k1
      if k1 ~= nil and k1 == k then
        return s.v1
      end
      local kv = s.kv
      if kv then
        local v = kv[k]
        if v ~= nil then
          return v
        end
      end
      local ps = s.pstate
      if ps then
        s = ps
      else
        o = s.proto
        if o == nil then
          return nil
        end
        return get(o, k, receiver)
      end
    end
  end
  return nil
end

-- internal: fast read of a watched key on `o`, returning `nil` in O(1) when neither `o`'s
-- plain prefix nor its ancestor proxy target's terminal plain chain defines the key
function M._getWatched(o, k)
  local s = state[o]
  local wb = watchedKeys[k]
  if wb and s then
    local ss
    if s.handler then
      ss = s
    elseif s.wepoch == watchEpoch and (s.wmask & wb) == 0 and s.strEpoch == strSkipEpoch then
      ss = s.strState
    end
    if ss and ss.passthroughWatched then
      local ts = state[ss.target]
      if ts and not ts.handler and ts.wepoch == watchEpoch and (ts.wmask & wb) == 0 then
        local tp = ts.pstate
        if tp == nil and ts.proto == nil then
          return nil
        end
        if
          tp
          and not tp.handler
          and tp.pstate == nil
          and tp.proto == nil
          and tp.wepoch == watchEpoch
          and (tp.wmask & wb) == 0
        then
          return nil
        end
      end
    end
  end
  return get(o, k, o)
end

function defineOwn(o, k, v)
  epoch.n = epoch.n + 1
  local s = state[o]
  if s then
    local handler = s.handler
    if not handler then
      plainSet(s, k, v)
      return true
    end
    local trap = handler.defineProperty
    if trap then
      local ctx = s.context
      if ctx ~= nil then
        return trap(ctx, s.target, k, v)
      end
      return trap(s.target, k, v)
    end
    return defineOwn(s.target, k, v)
  end
  if type(o) == "table" then
    rawset(o, k, v)
    return true
  end
  return false
end

function set(o, k, v, receiver)
  epoch.n = epoch.n + 1
  local s = state[o]
  if s then
    local handler = s.handler
    if not handler then
      if plainHas(s, k) or s.proto == nil then
        if receiver == o then
          plainSet(s, k, v)
          return true
        end
        return defineOwn(receiver, k, v)
      end
      return set(s.proto, k, v, receiver)
    else
      local trap = handler.set
      if trap then
        local ctx = s.context
        if ctx ~= nil then
          return trap(ctx, s.target, k, v, receiver)
        end
        return trap(s.target, k, v, receiver)
      end
      return set(s.target, k, v, receiver)
    end
  end
  if type(o) == "table" then
    local mt = getmetatable(o)
    local cls = classes[mt] and mt or nil
    if cls and rawget(o, k) == nil then
      local setter = classFindSetter(cls, k)
      if setter then
        setter(receiver, v)
        return true
      elseif setter == false then
        return false
      end
    end
    if receiver == o then
      if cls then
        rawset(o, k, v)
      else
        o[k] = v
      end
      return true
    end
    return defineOwn(receiver, k, v)
  end
  return false
end

function has(o, k)
  local s = state[o]
  while s and not s.handler do
    local k1 = s.k1
    if k1 ~= nil and k1 == k then
      return true
    end
    local kv = s.kv
    if kv and kv[k] ~= nil then
      return true
    end
    local ps = s.pstate
    if ps then
      s = ps
    else
      o = s.proto
      if o == nil then
        return false
      end
      s = nil
      break
    end
  end
  if s then
    local trap = s.handler.has
    if trap then
      local ctx = s.context
      if ctx ~= nil then
        return trap(ctx, s.target, k)
      end
      return trap(s.target, k)
    end
    return has(s.target, k)
  end
  if type(o) == "table" then
    local mt = getmetatable(o)
    if classes[mt] then
      return rawget(o, k) ~= nil or classDefines(mt, k)
    end
    return o[k] ~= nil
  end
  return false
end

M.get, M.set, M.has, M.defineOwn = get, set, has, defineOwn

local function hasOwn(o, k)
  local s = state[o]
  while s and s.handler do
    o = s.target
    s = state[o]
  end
  if s then
    return plainHas(s, k)
  end
  if type(o) == "table" then
    return rawget(o, k) ~= nil
  end
  return false
end
M.hasOwn = hasOwn

-- own value without walking the prototype chain
function M.getOwn(o, k)
  local s = state[o]
  while s and s.handler do
    o = s.target
    s = state[o]
  end
  if s then
    return plainGet(s, k)
  end
  if type(o) == "table" then
    return rawget(o, k)
  end
end

function M.delete(o, k)
  epoch.n = epoch.n + 1
  local s = state[o]
  if s then
    local handler = s.handler
    if not handler then
      return plainDelete(s, k)
    end
    local trap = handler.deleteProperty
    if trap then
      local ctx = s.context
      if ctx ~= nil then
        return trap(ctx, s.target, k)
      end
      return trap(s.target, k)
    end
    return M.delete(s.target, k)
  end
  if type(o) == "table" then
    rawset(o, k, nil)
    return true
  end
  return false
end

function M.getPrototypeOf(o)
  local s = state[o]
  while s and s.handler do
    o = s.target
    s = state[o]
  end
  if s then
    return s.proto
  end
  if type(o) == "table" then
    local mt = getmetatable(o)
    if classes[mt] then
      return mt
    end
  end
end

function M.setPrototypeOf(o, p)
  local s = state[o]
  assert(s and not s.handler, "setPrototypeOf: not a plain object")
  if p ~= nil then
    local cur = p
    local seen = {}
    while cur ~= nil and not seen[cur] do
      seen[cur] = true
      if rawequal(cur, o) then
        error("Cyclic __proto__ value", 2)
      end
      local cs = state[cur]
      if cs then
        cur = cs.handler and cs.target or cs.proto
      else
        cur = M.getPrototypeOf(cur)
      end
    end
  end
  epoch.n = epoch.n + 1
  watchEpoch = watchEpoch + 1
  protoEpoch.n = protoEpoch.n + 1
  ctxVersion.n = ctxVersion.n + 1
  local ps = p ~= nil and state[p] or nil
  if ps then
    ps.isProto = true
  end
  local newUnder = ps ~= nil and (ps.handler ~= nil or ps.underProxy == true) or nil
  if s.underProxy or newUnder then
    strSkipEpoch = strSkipEpoch + 1
  end
  s.proto = p
  s.pstate = ps
  s.underProxy = newUnder
  s.strState, s.strEpoch, s.strOwnOnly = nil, nil, nil
  s.wepoch = nil
  return o
end

function M.ownKeys(o)
  local s = state[o]
  while s and s.handler do
    o = s.target
    s = state[o]
  end
  if s then
    local k1 = s.k1
    if k1 ~= nil then
      return { k1 }
    end
    local own = s.own
    if not own then
      return {}
    end
    return own:keys()
  end
  local keys = {}
  if type(o) == "table" then
    for k in next, o do
      keys[#keys + 1] = k
    end
  end
  return keys
end

-- `for..in` order: own keys, then up the prototype chain, deduplicated
function M.keys(o)
  local result, seen = {}, {}
  local cur = o
  while cur ~= nil and isObject(cur) do
    for _, k in ipairs(M.ownKeys(cur)) do
      if not seen[k] then
        seen[k] = true
        result[#result + 1] = k
      end
    end
    cur = M.getPrototypeOf(cur)
  end
  return result
end

-- own entries as a Lua array of { key, value }
function M.entries(o)
  local r = {}
  for _, k in ipairs(M.ownKeys(o)) do
    r[#r + 1] = { k, M.getOwn(o, k) }
  end
  return r
end

-- `Object.create(proto)` + `Object.assign(own)`; `own` may be a Lua table
-- (keys in `pairs` order; pass a list of {k, v} via `entries` for order)
function M.create(proto, own)
  local o = setmetatable({}, OBJ)
  local ps = proto ~= nil and state[proto] or nil
  local strState, strEpoch, strOwnOnly, underProxy, wmask, wepoch = inheritChainMeta(ps)
  if own == nil then
    state[o] = {
      proto = proto,
      pstate = ps,
      underProxy = underProxy,
      strState = strState,
      strEpoch = strEpoch,
      strOwnOnly = strOwnOnly,
      wmask = wmask,
      wepoch = wepoch,
    }
    return o
  end
  local os = state[own]
  if os then
    while os and os.handler do
      own = os.target
      os = state[own]
    end
    if os then
      if os.k1 ~= nil then
        local k1 = os.k1
        local hasStr = type(k1) == "string"
        if hasStr then
          if strEpoch ~= nil and (strOwnOnly == false or strOwnOnly == k1) then
            strOwnOnly = k1
          else
            strState, strEpoch, strOwnOnly = nil, nil, nil
          end
        elseif wmask ~= nil then
          local wb = watchedKeys[k1]
          if wb then
            wmask = wmask | wb
          end
        end
        state[o] = {
          proto = proto,
          pstate = ps,
          k1 = k1,
          v1 = os.v1,
          hasStr = hasStr or nil,
          underProxy = underProxy,
          strState = strState,
          strEpoch = strEpoch,
          strOwnOnly = strOwnOnly,
          wmask = wmask,
          wepoch = wepoch,
        }
        return o
      end
      if os.own and os.own._head then
        local ownMap = Map.new()
        local kv = {}
        local hasStr = false
        local node = os.own._head
        while node do
          local nk = node.k
          if type(nk) == "string" then
            hasStr = true
            if strEpoch ~= nil and (strOwnOnly == false or strOwnOnly == nk) then
              strOwnOnly = nk
            else
              strState, strEpoch, strOwnOnly = nil, nil, nil
            end
          elseif wmask ~= nil then
            local wb = watchedKeys[nk]
            if wb then
              wmask = wmask | wb
            end
          end
          kv[nk] = node.v
          ownMap:set(nk, node.v)
          node = node.next
        end
        state[o] = {
          proto = proto,
          pstate = ps,
          kv = kv,
          own = ownMap,
          hasStr = hasStr or nil,
          underProxy = underProxy,
          strState = strState,
          strEpoch = strEpoch,
          strOwnOnly = strOwnOnly,
          wmask = wmask,
          wepoch = wepoch,
        }
        return o
      end
      state[o] = {
        proto = proto,
        pstate = ps,
        underProxy = underProxy,
        strState = strState,
        strEpoch = strEpoch,
        strOwnOnly = strOwnOnly,
        wmask = wmask,
        wepoch = wepoch,
      }
      return o
    elseif type(own) == "table" then
      local k1, v1 = next(own)
      if k1 == nil then
        state[o] = {
          proto = proto,
          pstate = ps,
          underProxy = underProxy,
          strState = strState,
          strEpoch = strEpoch,
          strOwnOnly = strOwnOnly,
          wmask = wmask,
          wepoch = wepoch,
        }
        return o
      end
      local k2, v2 = next(own, k1)
      if k2 == nil then
        local hasStr = type(k1) == "string"
        if hasStr then
          if strEpoch ~= nil and (strOwnOnly == false or strOwnOnly == k1) then
            strOwnOnly = k1
          else
            strState, strEpoch, strOwnOnly = nil, nil, nil
          end
        elseif wmask ~= nil then
          local wb = watchedKeys[k1]
          if wb then
            wmask = wmask | wb
          end
        end
        state[o] = {
          proto = proto,
          pstate = ps,
          k1 = k1,
          v1 = v1,
          hasStr = hasStr or nil,
          underProxy = underProxy,
          strState = strState,
          strEpoch = strEpoch,
          strOwnOnly = strOwnOnly,
          wmask = wmask,
          wepoch = wepoch,
        }
        return o
      end
      local s1, s2 = type(k1) == "string", type(k2) == "string"
      local hasStr = s1 or s2
      if s1 then
        if strEpoch ~= nil and (strOwnOnly == false or strOwnOnly == k1) then
          strOwnOnly = k1
        else
          strState, strEpoch, strOwnOnly = nil, nil, nil
        end
      end
      if s2 then
        if strEpoch ~= nil and (strOwnOnly == false or strOwnOnly == k2) then
          strOwnOnly = k2
        else
          strState, strEpoch, strOwnOnly = nil, nil, nil
        end
      end
      if wmask ~= nil then
        local wb1 = watchedKeys[k1]
        if wb1 then
          wmask = wmask | wb1
        end
        local wb2 = watchedKeys[k2]
        if wb2 then
          wmask = wmask | wb2
        end
      end
      local ownMap = Map.new()
      local kv = { [k1] = v1, [k2] = v2 }
      ownMap:set(k1, v1)
      ownMap:set(k2, v2)
      local k, v = next(own, k2)
      while k ~= nil do
        if type(k) == "string" then
          hasStr = true
          if strEpoch ~= nil and (strOwnOnly == false or strOwnOnly == k) then
            strOwnOnly = k
          else
            strState, strEpoch, strOwnOnly = nil, nil, nil
          end
        elseif wmask ~= nil then
          local wb = watchedKeys[k]
          if wb then
            wmask = wmask | wb
          end
        end
        kv[k] = v
        ownMap:set(k, v)
        k, v = next(own, k)
      end
      state[o] = {
        proto = proto,
        pstate = ps,
        kv = kv,
        own = ownMap,
        hasStr = hasStr or nil,
        underProxy = underProxy,
        strState = strState,
        strEpoch = strEpoch,
        strOwnOnly = strOwnOnly,
        wmask = wmask,
        wepoch = wepoch,
      }
      return o
    end
    state[o] = {
      proto = proto,
      pstate = ps,
      underProxy = underProxy,
      strState = strState,
      strEpoch = strEpoch,
      strOwnOnly = strOwnOnly,
      wmask = wmask,
      wepoch = wepoch,
    }
    return o
  end
  if getmetatable(own) == nil then
    local k1, v1 = next(own)
    if k1 == nil then
      state[o] = {
        proto = proto,
        pstate = ps,
        underProxy = underProxy,
        strState = strState,
        strEpoch = strEpoch,
        strOwnOnly = strOwnOnly,
        wmask = wmask,
        wepoch = wepoch,
      }
      return o
    end
    local k2, v2 = next(own, k1)
    if k2 == nil then
      local hasStr = type(k1) == "string"
      if hasStr then
        if strEpoch ~= nil and (strOwnOnly == false or strOwnOnly == k1) then
          strOwnOnly = k1
        else
          strState, strEpoch, strOwnOnly = nil, nil, nil
        end
      elseif wmask ~= nil then
        local wb = watchedKeys[k1]
        if wb then
          wmask = wmask | wb
        end
      end
      state[o] = {
        proto = proto,
        pstate = ps,
        k1 = k1,
        v1 = v1,
        hasStr = hasStr or nil,
        underProxy = underProxy,
        strState = strState,
        strEpoch = strEpoch,
        strOwnOnly = strOwnOnly,
        wmask = wmask,
        wepoch = wepoch,
      }
      return o
    end
    local s1, s2 = type(k1) == "string", type(k2) == "string"
    local hasStr = s1 or s2
    if s1 then
      if strEpoch ~= nil and (strOwnOnly == false or strOwnOnly == k1) then
        strOwnOnly = k1
      else
        strState, strEpoch, strOwnOnly = nil, nil, nil
      end
    end
    if s2 then
      if strEpoch ~= nil and (strOwnOnly == false or strOwnOnly == k2) then
        strOwnOnly = k2
      else
        strState, strEpoch, strOwnOnly = nil, nil, nil
      end
    end
    if wmask ~= nil then
      local wb1 = watchedKeys[k1]
      if wb1 then
        wmask = wmask | wb1
      end
      local wb2 = watchedKeys[k2]
      if wb2 then
        wmask = wmask | wb2
      end
    end
    local ownMap = Map.new()
    local kv = { [k1] = v1, [k2] = v2 }
    ownMap:set(k1, v1)
    ownMap:set(k2, v2)
    local k, v = next(own, k2)
    while k ~= nil do
      if type(k) == "string" then
        hasStr = true
        if strEpoch ~= nil and (strOwnOnly == false or strOwnOnly == k) then
          strOwnOnly = k
        else
          strState, strEpoch, strOwnOnly = nil, nil, nil
        end
      elseif wmask ~= nil then
        local wb = watchedKeys[k]
        if wb then
          wmask = wmask | wb
        end
      end
      kv[k] = v
      ownMap:set(k, v)
      k, v = next(own, k)
    end
    state[o] = {
      proto = proto,
      pstate = ps,
      kv = kv,
      own = ownMap,
      hasStr = hasStr or nil,
      underProxy = underProxy,
      strState = strState,
      strEpoch = strEpoch,
      strOwnOnly = strOwnOnly,
      wmask = wmask,
      wepoch = wepoch,
    }
    return o
  end
  local s = {
    proto = proto,
    pstate = ps,
    underProxy = underProxy,
    strState = strState,
    strEpoch = strEpoch,
    strOwnOnly = strOwnOnly,
    wmask = wmask,
    wepoch = wepoch,
  }
  for k, v in pairs(own) do
    plainSet(s, k, v)
  end
  state[o] = s
  return o
end

-- internal: create a plain object with one initial own property (no temp table, no epoch bump)
function M._createSingle(proto, k, v)
  local o = setmetatable({}, OBJ)
  local ps = proto ~= nil and state[proto] or nil
  local strState, strEpoch, strOwnOnly, underProxy, wmask, wepoch = inheritChainMeta(ps)
  if v ~= nil then
    local hasStr = type(k) == "string"
    if hasStr then
      if strEpoch ~= nil and (strOwnOnly == false or strOwnOnly == k) then
        strOwnOnly = k
      else
        strState, strEpoch, strOwnOnly = nil, nil, nil
      end
    elseif wmask ~= nil then
      local wb = watchedKeys[k]
      if wb then
        wmask = wmask | wb
      end
    end
    state[o] = {
      proto = proto,
      pstate = ps,
      k1 = k,
      v1 = v,
      hasStr = hasStr or nil,
      underProxy = underProxy,
      strState = strState,
      strEpoch = strEpoch,
      strOwnOnly = strOwnOnly,
      wmask = wmask,
      wepoch = wepoch,
    }
  else
    state[o] = {
      proto = proto,
      pstate = ps,
      underProxy = underProxy,
      strState = strState,
      strEpoch = strEpoch,
      strOwnOnly = strOwnOnly,
      wmask = wmask,
      wepoch = wepoch,
    }
  end
  return o
end

-- `Object.assign(target, ...sources)` (own keys of objects, pairs of tables)
function M.assign(target, ...)
  for i = 1, select("#", ...) do
    local src = select(i, ...)
    if src ~= nil then
      if isObject(src) then
        for _, k in ipairs(M.ownKeys(src)) do
          set(target, k, M.getOwn(src, k), target)
        end
      else
        for k, v in pairs(src) do
          set(target, k, v, target)
        end
      end
    end
  end
  return target
end

-- `new Proxy(target, handler)`; handler traps: get(target, k, receiver),
-- set(target, k, v, receiver), has(target, k), apply(target, thisArg, ...),
-- defineProperty(target, k, v), deleteProperty(target, k)
function M.proxy(target, handler)
  local o = setmetatable({}, PROXY_OBJ)
  state[o] = { target = target, handler = handler }
  return o
end

-- internal: proxy with a shared handler table whose traps receive `context` as first argument
function M._proxyWithContext(target, handler, context, passthroughWatched)
  local o = setmetatable({}, PROXY_OBJ)
  state[o] = {
    target = target,
    handler = handler,
    context = context,
    get = handler.get,
    apply = handler.apply,
    passthroughWatched = passthroughWatched,
  }
  return o
end

function M.isProxy(o)
  local s = state[o]
  return s ~= nil and s.handler ~= nil
end

function M.proxyTarget(o)
  local s = state[o]
  return s and s.target
end

-- is `x` callable (function, or a table with `__call`)?
local function callable(x)
  if type(x) == "function" then
    return true
  end
  local s = state[x]
  if s then
    if s.handler then
      return s.handler.apply ~= nil or callable(s.target)
    end
    local p = s.proto
    local ps = s.pstate
    while ps and not ps.handler do
      p = ps.proto
      ps = ps.pstate
    end
    return p ~= nil and callable(p)
  end
  local mt = getmetatable(x)
  return type(mt) == "table" and rawget(mt, "__call") ~= nil
end
M.callable = callable

-- `Reflect.apply(fn, thisArg, args)`; `self` is the object being called
-- (relevant for objects whose prototype is a callable class instance)
local function apply(fn, self, ...)
  local s = state[fn]
  if s then
    local handler = s.handler
    if handler then
      local trap = handler.apply
      if trap then
        local ctx = s.context
        if ctx ~= nil then
          return trap(ctx, s.target, nil, ...)
        end
        return trap(s.target, nil, ...)
      end
      return apply(s.target, self, ...)
    end
    -- a plain object inherits callability from its prototype chain
    local p = s.proto
    local ps = s.pstate
    while ps and not ps.handler do
      p = ps.proto
      ps = ps.pstate
    end
    if p == nil then
      error("attempt to call a non-callable object", 2)
    end
    return apply(p, self, ...)
  end
  if type(fn) == "function" then
    return fn(...)
  end
  local mt = getmetatable(fn)
  local call = type(mt) == "table" and rawget(mt, "__call")
  if call then
    return call(self, ...)
  end
  error("attempt to call a " .. type(fn) .. " value", 2)
end
M.apply = apply

OBJ.__index = function(o, k)
  local s = state[o]
  local receiver = o
  if type(k) == "string" then
    if s.strEpoch == strSkipEpoch then
      if k == s.k1 then
        return s.v1
      end
      if k ~= s.strOwnOnly then
        local ps = s.strState
        local trap = ps.get or ps.handler.get
        if trap then
          local ctx = ps.context
          if ctx ~= nil then
            return trap(ctx, ps.target, k, o)
          end
          return trap(ps.target, k, o)
        end
        return get(ps.target, k, o)
      end
    end
    local orig = s
    local canRearm = true
    local rearmKey = false
    while true do
      local k1 = s.k1
      if k1 ~= nil then
        if k1 == k then
          return s.v1
        end
        if canRearm and type(k1) == "string" then
          if rearmKey == false or rearmKey == k1 then
            rearmKey = k1
          else
            canRearm = false
          end
        end
      else
        local kv = s.kv
        if kv then
          local v = kv[k]
          if v ~= nil then
            return v
          end
          if canRearm and s.hasStr then
            canRearm = false
          end
        end
      end
      local ps = s.pstate
      if ps then
        if ps.handler then
          if canRearm then
            orig.strState, orig.strEpoch, orig.strOwnOnly, orig.underProxy = ps, strSkipEpoch, rearmKey, true
            local cur = orig.pstate
            while cur and cur ~= ps do
              cur.isProto = true
              cur.underProxy = true
              cur = cur.pstate
            end
          end
          local trap = ps.get or ps.handler.get
          if trap then
            local ctx = ps.context
            if ctx ~= nil then
              return trap(ctx, ps.target, k, receiver)
            end
            return trap(ps.target, k, receiver)
          end
          return get(ps.target, k, receiver)
        end
        s = ps
      else
        o = s.proto
        if o == nil then
          return nil
        end
        break
      end
    end
  else
    while true do
      local k1 = s.k1
      if k1 ~= nil and k1 == k then
        return s.v1
      end
      local kv = s.kv
      if kv then
        local v = kv[k]
        if v ~= nil then
          return v
        end
      end
      local ps = s.pstate
      if ps then
        if ps.handler then
          local trap = ps.get or ps.handler.get
          if trap then
            local ctx = ps.context
            if ctx ~= nil then
              return trap(ctx, ps.target, k, receiver)
            end
            return trap(ps.target, k, receiver)
          end
          return get(ps.target, k, receiver)
        end
        s = ps
      else
        o = s.proto
        if o == nil then
          return nil
        end
        break
      end
    end
  end
  local t = type(o)
  if t == "table" then
    local mt = getmetatable(o)
    if classes[mt] then
      local v = rawget(o, k)
      if v ~= nil then
        return v
      end
      return classLookup(mt, k, receiver)
    end
    return o[k]
  elseif t == "string" then
    return string[k]
  end
  return nil
end

PROXY_OBJ.__index = function(o, k)
  local s = state[o]
  local trap = s.get or s.handler.get
  if trap then
    local ctx = s.context
    if ctx ~= nil then
      return trap(ctx, s.target, k, o)
    end
    return trap(s.target, k, o)
  end
  return get(s.target, k, o)
end

local function objNewindex(o, k, v)
  set(o, k, v, o)
end
OBJ.__newindex = objNewindex
PROXY_OBJ.__newindex = objNewindex

OBJ.__call = function(o, ...)
  return apply(o, o, ...)
end
PROXY_OBJ.__call = function(o, ...)
  local s = state[o]
  local trap = s.apply or s.handler.apply
  if trap then
    local ctx = s.context
    if ctx ~= nil then
      return trap(ctx, s.target, nil, ...)
    end
    return trap(s.target, nil, ...)
  end
  return apply(s.target, o, ...)
end

local function objTostring(o)
  local f = get(o, M.inspect, o)
  if f then
    return f(o)
  end
  local s = state[o]
  if s and s.handler then
    return tostring(s.target)
  end
  return "Object"
end
OBJ.__tostring = objTostring
PROXY_OBJ.__tostring = objTostring

return M
