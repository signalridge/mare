-- Ported from cordis packages/core/src/index.ts (MIT, (c) Shigma)
local future = require("cordis.future")
local symbols = require("cordis.symbols")
local class = require("cordis.class")
local proto = require("cordis.proto")
local errors = require("cordis.error")
local config = require("cordis.config")

local cordis = {}

-- async primitives
cordis.Future = future.Future
cordis.future = future
cordis.async = future.async
cordis.await = future.await
cordis.tick = future.tick

-- symbols and null
cordis.symbol = symbols.symbol
cordis.symbolFor = symbols.symbolFor
cordis.isSymbol = symbols.isSymbol
cordis.symbols = symbols.symbols
cordis.null = symbols.null
cordis.isNullable = symbols.isNullable

-- classes and objects
cordis.class = setmetatable({ is = class.isClass }, {
  __call = function(_, name, parent)
    return class.class(name, parent)
  end,
})
cordis.proto = proto
cordis.ordered = require("cordis.ordered")
cordis.DisposableList = require("cordis.list")

-- errors
cordis.Error = errors.Error
cordis.TypeError = errors.TypeError
cordis.AggregateError = errors.AggregateError
cordis.CordisError = errors.CordisError
cordis.ValidationError = config.ValidationError
cordis.resolveConfig = config.resolveConfig

-- core
local fiber = require("cordis.fiber")
local events = require("cordis.events")
local registry = require("cordis.registry")
local reflect = require("cordis.reflect")
local logger = require("cordis.logger")
local trace = require("cordis.trace")

cordis.Context = require("cordis.context")
cordis.Fiber = fiber.Fiber
cordis.FiberState = fiber.FiberState
cordis.iter = fiber.iter
cordis.asyncIter = fiber.asyncIter
cordis.Service = require("cordis.service")
cordis.Inject = registry.Inject
cordis.RegistryService = registry.RegistryService
cordis.ReflectService = reflect.ReflectService
cordis.EventsService = events.EventsService
cordis.isBailed = events.isBailed
cordis.locate = reflect.locate
cordis.Logger = logger.Logger
cordis.LoggerService = logger.LoggerService
cordis.LoggerLevel = logger.LoggerLevel
cordis.composeError = trace.composeError
cordis.buildOuterStack = trace.buildOuterStack
cordis.getTraceable = trace.getTraceable

return cordis
