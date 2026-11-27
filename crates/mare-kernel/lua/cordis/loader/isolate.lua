-- Ported from cordis packages/loader/src/config/isolate.ts (MIT, (c) Shigma)
--
-- Isolate maps are JS-like objects (`cordis.proto`); service symbols come
-- from `cordis.symbol` / `cordis.symbolFor`.
local cordis = require("cordis")
local utils = require("cordis.loader.utils")
local proto = cordis.proto
local ordered = cordis.ordered

local M = {}

local function noop() end

local function isPlainProtoChain(o)
  while o ~= nil do
    local s = proto._state[o]
    if not s or s.handler ~= nil then
      return false
    end
    o = s.proto
  end
  return true
end

local LocalRealm = cordis.class("LocalRealm")
M.LocalRealm = LocalRealm

function LocalRealm:constructor(entry)
  self.entry = entry
  self.store = {}
end

function LocalRealm:access(key)
  local symbol = self.store[key]
  if not symbol then
    symbol = cordis.symbol(key .. "#" .. self.entry.options.id)
    self.store[key] = symbol
  end
  return symbol
end

local function quote(value)
  if value:match("^[%w_]*$") then
    return value
  end
  return string.format("%q", value)
end

---The symbol of a service in a global realm, shared by every entry with the same label.
function M.accessGlobal(key, label)
  return cordis.symbolFor("cordis.isolate." .. quote(key) .. "@" .. quote(label))
end

---Whether `table` resolves `name` through `boundary`, that is, `boundary` is
---in the prototype chain of `table` and no table between them maps `name`.
local function within(table, boundary, name)
  while table ~= nil do
    if rawequal(table, boundary) then
      return true
    end
    if proto.hasOwn(table, name) then
      return false
    end
    table = proto.getPrototypeOf(table)
  end
  return false
end

-- `Object.entries(entry.options.isolate ?? {})`; a plain table has no key
-- order, so its names are sorted
local function isolateEntries(isolate)
  local result = {}
  if isolate == nil or isolate == cordis.null then
    return result
  end
  if proto.is(isolate) then
    for _, name in ipairs(proto.ownKeys(isolate)) do
      result[#result + 1] = { name, proto.getOwn(isolate, name) }
    end
    return result
  end
  for name, label in pairs(isolate) do
    result[#result + 1] = { name, label }
  end
  table.sort(result, function(a, b)
    return tostring(a[1]) < tostring(b[1])
  end)
  return result
end

---Apply the isolation of an entry to its context. `next` reloads the entry in
---the new isolation (it returns a Future), after which the impls that go with
---the entry are transferred and the affected fibers are notified.
---The prelude (steps 1-4 up to `next()`) runs synchronously, like the part of
---the upstream `async` function before its first `await`.
function M.patchIsolate(entry, next)
  local Context = cordis.Context
  local host = entry.ctx.host
  local isolateOption = entry.options.isolate
  local oldMap = entry.ctx[Context.isolate]
  local parentIsolate = entry.parent.ctx[Context.isolate]

  if isolateOption == nil then
    local s = proto._state[oldMap]
    if
      s
      and s.handler == nil
      and s.k1 == nil
      and (s.own == nil or s.own._size == 0)
      and rawequal(s.proto, parentIsolate)
      and isPlainProtoChain(parentIsolate)
    then
      return utils.try(host, next):next(noop)
    end
  end

  -- step 1: generate new isolate map
  local newMap = proto.create(parentIsolate)
  for _, pair in ipairs(isolateEntries(isolateOption)) do
    local name, label = pair[1], pair[2]
    if label == true then
      entry.realm = entry.realm or LocalRealm.new(entry)
      newMap[name] = entry.realm:access(name)
    else
      newMap[name] = M.accessGlobal(name, label)
    end
  end

  -- step 2: generate service diff
  -- `inside`: whether the provider resolves the name through the entry
  local diff = ordered.Map.new()
  -- `for...in` includes the names inherited from the old and new parents
  local names = ordered.Set.new()
  for _, name in ipairs(proto.keys(oldMap)) do
    names:add(name)
  end
  for _, name in ipairs(proto.keys(newMap)) do
    names:add(name)
  end
  local store = entry.ctx.reflect.store
  for _, name in ipairs(names:values()) do
    local old, new = oldMap[name], newMap[name]
    if not rawequal(new, old) then
      for i = 1, 2 do
        local symbol = i == 1 and old or new
        local impl = symbol and store[symbol]
        if impl then
          if not impl.ctx.fiber then
            entry.ctx.logger:warn(cordis.Error.new("expected service " .. tostring(name) .. " to be implemented"))
          else
            -- the provision context, which may isolate the name apart from its fiber
            local inside = within(impl.ctx[Context.isolate], oldMap, name)
            diff:set(name, { old, new, inside })
            if not inside then
              break
            end
          end
        end
      end
    end
  end

  -- step 3: set prototype for transferred context
  proto.setPrototypeOf(entry.ctx[Context.isolate], entry.parent.ctx[Context.isolate])
  utils.swap(entry.ctx[Context.isolate], newMap)

  -- step 4: reload fiber
  return utils.try(host, next):next(function()
    -- step 5: replace service impl
    for _, symbols in ipairs(diff:values()) do
      local symbol1, symbol2, inside = symbols[1], symbols[2], symbols[3]
      local impl = symbol1 and store[symbol1]
      if inside and impl and symbol2 and not store[symbol2] then
        store[symbol2] = impl
        store[symbol1] = nil
        impl.isolate = symbol2
      end
    end

    -- step 6: reflect notify
    -- The fibers on each side of the transfer are notified from a context that
    -- resolves the names to that side, which reports the service of that side.
    -- The entry's isolate map is swapped in place, so `oldMap` is still the
    -- boundary; a context is affected if it lies on the other side of it.
    local keys = diff:keys()
    -- no name moved, nothing to notify (a notify walks every fiber)
    if #keys == 0 then
      return
    end
    for side = 1, 2 do
      local notifier = entry.ctx
      for _, name in ipairs(keys) do
        local symbol = diff:get(name)[side]
        if symbol then
          notifier = notifier:isolate(name, symbol)
        end
      end
      notifier.reflect:notify(keys, function(ctx, name)
        local symbols = diff:get(name)
        local symbol = symbols[side]
        return not not symbol
          and rawequal(ctx[Context.isolate][name], symbol)
          and within(ctx[Context.isolate], oldMap, name) ~= symbols[3]
      end)
    end
  end)
end

return M
