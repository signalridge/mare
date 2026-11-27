-- Ported from cordis packages/core/src/registry.ts (MIT, (c) Shigma)
local proto = require("cordis.proto")
local classlib = require("cordis.class")
local class = classlib.class
local symbols = require("cordis.symbols")
local S = symbols.symbols
local errors = require("cordis.error")
local trace = require("cordis.trace")
local Map = require("cordis.ordered").Map
local DisposableList = require("cordis.list")

local M = {}

local function isApplicable(object)
  return type(object) == "table" and not classlib.isClass(object) and proto.callable(proto.get(object, "apply", object))
end

local function jsTypeof(x)
  local t = type(x)
  if t == "nil" then
    return "undefined"
  end
  if t == "table" then
    return proto.callable(x) and "function" or "object"
  end
  return t
end

local function invalid(component)
  return errors.Error.new(
    'invalid component, expect function or object with an "apply" method, received ' .. jsTypeof(component)
  )
end

-- Inject ------------------------------------------------------------------------

local Inject = {}
M.Inject = Inject

local function isSequence(t)
  return type(t) == "table" and not proto.is(t) and t[1] ~= nil
end

---`Inject.resolve(inject, result)`: name -> config (`cordis.null` for none),
---in declaration order (a plain-table dict is ordered by key).
function Inject.resolve(inject, result)
  result = result or proto.create(nil)
  if not inject or inject == symbols.null then
    return result
  end
  if isSequence(inject) then
    for _, name in ipairs(inject) do
      result[name] = symbols.null
    end
  elseif proto.is(inject) then
    if proto.has(inject, S.checkProto) then
      proto.assign(result, Inject.resolve(proto.getPrototypeOf(inject)))
    end
    for _, name in ipairs(proto.ownKeys(inject)) do
      if type(name) == "string" then
        local v = proto.getOwn(inject, name)
        result[name] = v == nil and symbols.null or v
      end
    end
  else
    local names = {}
    for k in pairs(inject) do
      if type(k) == "string" then
        names[#names + 1] = k
      end
    end
    table.sort(names)
    for _, name in ipairs(names) do
      result[name] = inject[name]
    end
  end
  return result
end

---`@Inject(name, config)` on a class (called as a function).
function Inject.class(cls, name, config)
  if rawget(cls, "inject") == nil or not proto.is(rawget(cls, "inject")) then
    local parent = rawget(cls, "super")
    local base = parent and parent.inject
    if base ~= nil and not proto.is(base) then
      base = Inject.resolve(base)
    end
    local inject = proto.create(base)
    -- an inject declared as a plain table before the decorator keeps its entries
    local own = rawget(cls, "inject")
    if own then
      proto.assign(inject, Inject.resolve(own))
    end
    proto.defineOwn(inject, S.checkProto, true)
    rawset(cls, "inject", inject)
  end
  cls.inject[name] = config == nil and symbols.null or config
end

-- the dependencies declared on a method, shared by every `@Inject` on it
-- (registry.ts:30: `value[symbols.decorator].inject`)
local methodInjects = setmetatable({}, { __mode = "k" })

---`@Inject(name, config)` on a class method. Declarations on one method merge
---their dependencies; each declaration adds one initializer, as upstream.
function Inject.method(cls, methodName, deps)
  local value = cls[methodName]
  local inject = methodInjects[value]
  if not inject then
    inject = proto.create(nil)
    methodInjects[value] = inject
  end
  Inject.resolve(deps, inject)
  -- runs on every instance once it is constructed (decorator.addInitializer)
  local initializers = rawget(cls, "__initializers") or {}
  rawset(cls, "__initializers", initializers)
  initializers[#initializers + 1] = function(self)
    local hooks = rawget(self, S.initHooks) or {}
    rawset(self, S.initHooks, hooks)
    hooks[#hooks + 1] = function()
      local tracker = proto.get(self, S.tracker, self)
      local property = tracker and tracker.property
      self.ctx:inject(inject, function(ctx)
        local receiver = self
        if property then
          receiver = trace.withProps(self, { [property] = ctx })
        end
        return value(receiver)
      end)
    end
  end
end

---The init hooks of a component instance (fiber.ts:158-163): the initializers
---of its class chain run first (base class first), for any class.
function M.initHooks(instance)
  local cls = type(instance) == "table" and classlib.classOf(classlib.unwrap and classlib.unwrap(instance) or instance)
  if cls then
    local chain = {}
    local c = cls
    while c do
      table.insert(chain, 1, c)
      c = rawget(c, "super")
    end
    for _, k in ipairs(chain) do
      for _, init in ipairs(rawget(k, "__initializers") or {}) do
        init(instance)
      end
    end
  end
  return proto.get(instance, S.initHooks, instance)
end

setmetatable(Inject, {
  __call = function(_, cls, name, config)
    return Inject.class(cls, name, config)
  end,
})

-- RegistryService ---------------------------------------------------------------

local RegistryService = class("RegistryService")
M.RegistryService = RegistryService

function RegistryService:constructor(ctx)
  self.ctx = ctx
  self._counter = 0
  self._internal = Map.new()
  self._aliases = setmetatable({}, { __mode = "k" })
  rawset(self, S.tracker, { property = "ctx", noShadow = true })
end

RegistryService.__get.counter = function(self)
  self._counter = self._counter + 1
  return self._counter
end

RegistryService.__get.size = function(self)
  return self._internal:size()
end

function RegistryService:resolve(component)
  -- component.apply may throw
  local ok, result = pcall(function()
    if classlib.isClass(component) or type(component) == "function" then
      return component
    end
    if type(component) == "table" and proto.callable(component) then
      return component
    end
    if isApplicable(component) then
      return component.apply
    end
  end)
  if ok then
    return result
  end
end

function RegistryService:_forward(component)
  local key = self:resolve(component)
  while key do
    local nxt = self._aliases[key]
    if not nxt then
      return key, component
    end
    component = nxt
    key = self:resolve(nxt)
  end
end

function RegistryService:_create(callback, component)
  local name = proto.get(component, "name", component)
  if name == "apply" or type(name) ~= "string" then
    name = nil
  end
  local runtime = {
    name = name,
    callback = callback,
    fibers = DisposableList.new(),
    Config = proto.get(component, "Config", component),
  }
  self._internal:set(callback, runtime)
  return runtime
end

function RegistryService:get(component)
  local key = self:_forward(component)
  return key and self._internal:get(key)
end

function RegistryService:has(component)
  local key = self:_forward(component)
  return key ~= nil and self._internal:has(key)
end

function RegistryService:_unregister(runtime)
  self._internal:delete(runtime.callback)
end

function RegistryService:replace(oldComponent, newComponent)
  local oldKey = self:_forward(oldComponent)
  if not oldKey then
    error(invalid(oldComponent), 2)
  end
  local key = self:resolve(newComponent)
  if not key then
    error(invalid(newComponent), 2)
  end

  -- `next` is an implementation of its own again
  self._aliases[key] = nil
  local runtime = self._internal:get(oldKey)
  local target = self._internal:get(key)
  if oldKey ~= key then
    self._internal:delete(oldKey)
    self._aliases[oldKey] = newComponent
  end

  local fibers = runtime and runtime.fibers:values() or {}
  if #fibers == 0 then
    return fibers
  end
  target = target or self:_create(key, newComponent)

  for _, fiber in ipairs(fibers) do
    fiber:_replace(target, Inject.resolve(proto.get(newComponent, "inject", newComponent)))
  end
  return fibers
end

local protoState = proto._state
local getmetatable = getmetatable
local rawget = rawget

local function unwrapRegistry(self)
  local s = protoState[self]
  if s and s.handler then
    local raw = s.target
    if getmetatable(raw) == RegistryService then
      local tc = s.context
      local ctx = tc and tc.tracker and tc.tracker.property == "ctx" and tc.useSite or self.ctx
      return raw, ctx
    end
  elseif getmetatable(self) == RegistryService then
    return self, rawget(self, "ctx")
  end
  return self, self.ctx
end

function RegistryService:keys()
  local raw = unwrapRegistry(self)
  return raw._internal:keys()
end

function RegistryService:values()
  local raw = unwrapRegistry(self)
  return raw._internal:values()
end

function RegistryService:entries()
  local raw = unwrapRegistry(self)
  return raw._internal:entries()
end

function RegistryService:forEach(callback)
  local raw = unwrapRegistry(self)
  return raw._internal:forEach(callback)
end

function RegistryService:inject(inject, callback)
  return self:use({ inject = inject, apply = callback })
end

local function normalizeUseOptions(options)
  local opts = {}
  if type(options) == "table" then
    for k, v in pairs(options) do
      opts[k] = v
    end
  end
  if opts.getOuterStack == nil then
    opts.getOuterStack = trace.buildOuterStack()
  end
  return opts
end

---@deprecated Use `registry:use()` instead.
function RegistryService:plugin(component, config, options)
  return self:use(component, config, normalizeUseOptions(options))
end

function RegistryService:use(component, config, options)
  local opts = normalizeUseOptions(options)
  local raw, ctx = unwrapRegistry(self)
  -- check if it's a valid component
  local callback, target = raw:_forward(component)
  if not callback then
    error(invalid(component), 2)
  end
  ctx.fiber:assertActive()

  local runtime = raw._internal:get(callback) or raw:_create(callback, target)
  local Fiber = require("cordis.fiber").Fiber
  return Fiber.new(ctx, config, Inject.resolve(proto.get(target, "inject", target)), runtime, opts)
end

return M
