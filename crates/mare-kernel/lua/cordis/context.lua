-- Ported from cordis packages/core/src/context.ts (MIT, (c) Shigma)
local proto = require("cordis.proto")
local symbols = require("cordis.symbols")
local S = symbols.symbols
local trace = require("cordis.trace")

local Context = {}

Context.root = S.root
Context.effect = S.effect
Context.filter = S.filter
Context.isolate = S.isolate
Context.intercept = S.intercept

local IS = symbols.symbolFor("cordis.is")

-- `Context.prototype`
local ContextProto = proto.create(nil)
Context.prototype = ContextProto
proto.defineOwn(ContextProto, IS, true)
-- `Context.prototype.constructor`
proto.defineOwn(ContextProto, "constructor", Context)

function Context.is(value)
  if type(value) ~= "table" or symbols.isSymbol(value) then
    return false
  end
  return not not proto.get(value, IS, value)
end

---`name in ctx`
function Context.has(ctx, name)
  return proto.has(ctx, name)
end

function Context.new(options)
  local host = options and options.host
  assert(type(host) == "table" and host.spawn and host.latch, "Context.new: options.host (cordis.Host) is required")
  -- contexts carry the host's facade (future.facade), not the host
  host = require("cordis.future").facade(host)
  local Fiber = require("cordis.fiber").Fiber
  local reflect = require("cordis.reflect")
  local RegistryService = require("cordis.registry").RegistryService
  local EventsService = require("cordis.events").EventsService
  local LoggerService = require("cordis.logger").LoggerService

  local target = proto.create(ContextProto)
  proto.defineOwn(target, S.isolate, proto.create(nil))
  proto.defineOwn(target, S.intercept, proto.create(nil))
  local self = reflect._createRootProxy(target, ContextProto)
  proto.defineOwn(target, S.root, self)
  proto.defineOwn(target, "host", host)
  local fiber = Fiber.new(self, {}, proto.create(nil), nil)
  proto.defineOwn(target, "fiber", fiber)
  proto.defineOwn(target, "reflect", reflect.ReflectService.new(self))
  proto.defineOwn(target, "registry", RegistryService.new(self))
  proto.defineOwn(target, "events", EventsService.new(self))
  proto.defineOwn(target, "logger", LoggerService.new(self))
  fiber._disposables:clear()
  return self
end

proto.defineOwn(ContextProto, proto.inspect, function(self)
  return "Context <" .. self.fiber.name .. ">"
end)

proto.defineOwn(ContextProto, "extend", function(self, meta)
  local shadow = proto.getOwn(self, S.shadow)
  local parent = shadow and proto.getPrototypeOf(self) or trace.getTraceable(self, self)
  local child = proto.create(parent, meta)
  if not shadow then
    return child
  end
  return proto._createSingle(child, S.shadow, shadow)
end)

-- a terminal close of the root context.
-- Upstream's root dispose is a restart: the root stays usable. `close` marks
-- the root fiber disposed first, so nothing can register on it any more (not
-- even cleanup that runs during the close), unloads it like any fiber (every
-- effect and child fiber is disposed, asynchronous cleanup included), and
-- returns a Future that settles once that is done. Repeated calls return it.
local closing = setmetatable({}, { __mode = "k" })

proto.defineOwn(ContextProto, "close", function(self)
  local fiber = self.fiber
  if fiber.runtime then
    error(require("cordis.error").Error.new("only the root context can be closed"), 2)
  end
  local pending = closing[fiber]
  if pending then
    return pending
  end
  local fiberlib = require("cordis.fiber")
  fiber.uid = nil
  pending = fiber:await():_next(function() end)
  closing[fiber] = pending
  fiber:_setEpoch(fiberlib.INACTIVE)
  return pending
end)

proto.defineOwn(ContextProto, "isolate", function(self, name, label)
  local shadow = proto.create(self[S.isolate])
  shadow[name] = label or symbols.symbol(name)
  return self:extend({ [S.isolate] = shadow })
end)

proto.defineOwn(ContextProto, "intercept", function(self, name, config)
  local intercept = proto.create(self[S.intercept])
  intercept[name] = config
  return self:extend({ [S.intercept] = intercept })
end)

return Context
