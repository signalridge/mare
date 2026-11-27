-- Ported from cordis packages/core/src/logger.ts (MIT, (c) Shigma)
local proto = require("cordis.proto")
local future = require("cordis.future")
local classlib = require("cordis.class")
local class = classlib.class
local symbols = require("cordis.symbols")
local S = symbols.symbols
local errors = require("cordis.error")
local trace = require("cordis.trace")
local Map = require("cordis.ordered").Map

local M = {}

local LoggerLevel = { ERROR = 0, WARN = 1, INFO = 2, DEBUG = 3 }
M.LoggerLevel = LoggerLevel

local c16 = { 6, 2, 3, 4, 5, 1 }
local c256 = {
  20,
  21,
  26,
  27,
  32,
  33,
  38,
  39,
  40,
  41,
  42,
  43,
  44,
  45,
  56,
  57,
  62,
  63,
  68,
  69,
  74,
  75,
  76,
  77,
  78,
  79,
  80,
  81,
  92,
  93,
  98,
  99,
  112,
  113,
  129,
  134,
  135,
  148,
  149,
  160,
  161,
  162,
  163,
  164,
  165,
  166,
  167,
  168,
  169,
  170,
  171,
  172,
  173,
  178,
  179,
  184,
  185,
  196,
  197,
  198,
  199,
  200,
  201,
  202,
  203,
  204,
  205,
  206,
  207,
  208,
  209,
  214,
  215,
  220,
  221,
}
M.c16, M.c256 = c16, c256

-- cosmokit `hyphenate`
local function hyphenate(source)
  return (
    source:gsub("()([_A-Z])", function(index, char)
      if char == "_" then
        return "-"
      end
      return (index > 1 and "-" or "") .. char:lower()
    end)
  )
end
M.hyphenate = hyphenate

local function isError(x)
  return type(x) == "table" and errors.Error.is(x)
end

-- `JSON.stringify` (best effort)
local function stringify(value, seen)
  local t = type(value)
  if t == "nil" or value == symbols.null then
    return "null"
  end
  if t == "string" then
    return string.format("%q", value)
  end
  if t == "number" then
    if value ~= value or value == math.huge or value == -math.huge then
      return "null"
    end
    if math.type(value) == "float" and value == math.floor(value) and math.abs(value) < 2 ^ 53 then
      return string.format("%d", value)
    end
    return tostring(value)
  end
  if t == "boolean" then
    return tostring(value)
  end
  if t ~= "table" then
    return nil
  end
  seen = seen or {}
  if seen[value] then
    error(errors.TypeError.new("Converting circular structure to JSON"), 0)
  end
  seen[value] = true
  local parts = {}
  if proto.is(value) then
    for _, k in ipairs(proto.ownKeys(value)) do
      if type(k) == "string" then
        local s = stringify(value[k], seen)
        if s then
          parts[#parts + 1] = string.format("%q:%s", k, s)
        end
      end
    end
    seen[value] = nil
    return "{" .. table.concat(parts, ",") .. "}"
  end
  if value[1] ~= nil or next(value) == nil and getmetatable(value) == nil and false then
    for i = 1, #value do
      parts[i] = stringify(value[i], seen) or "null"
    end
    seen[value] = nil
    return "[" .. table.concat(parts, ",") .. "]"
  end
  local keys = {}
  for k in pairs(value) do
    if type(k) == "string" then
      keys[#keys + 1] = k
    end
  end
  table.sort(keys)
  for _, k in ipairs(keys) do
    local s = stringify(value[k], seen)
    if s then
      parts[#parts + 1] = string.format("%q:%s", k, s)
    end
  end
  seen[value] = nil
  return "{" .. table.concat(parts, ",") .. "}"
end
M.stringify = stringify

local function toNumber(value)
  if type(value) == "number" then
    return value
  end
  if type(value) == "boolean" then
    return value and 1 or 0
  end
  return tonumber(value) or 0 / 0
end

local function trunc(n)
  if n ~= n then
    return n
  end
  local i = n >= 0 and math.floor(n) or math.ceil(n)
  return math.tointeger(i) or i
end

local function jsString(value)
  if value == nil then
    return "undefined"
  end
  if value == symbols.null then
    return "null"
  end
  if type(value) == "number" then
    if value ~= value then
      return "NaN"
    end
    if math.type(value) == "float" and value == math.floor(value) and math.abs(value) < 2 ^ 53 then
      return string.format("%d", value)
    end
  end
  return tostring(value)
end

local Logger = class("Logger")
M.Logger = Logger

local defaultFormatters = {
  s = function(value)
    return jsString(value)
  end,
  d = function(value)
    return jsString(trunc(toNumber(value)))
  end,
  i = function(value)
    return jsString(trunc(toNumber(value)))
  end,
  f = function(value)
    return jsString(toNumber(value))
  end,
  o = function(value)
    return stringify(value) or jsString(value)
  end,
  O = function(value)
    return stringify(value) or jsString(value)
  end,
  c = function()
    return ""
  end,
  C = function(value, exporter, message)
    return Logger.color(exporter, Logger.code(message.name, exporter.colors), value)
  end,
}
M.defaultFormatters = defaultFormatters

function Logger.color(exporter, code, value, decoration)
  decoration = decoration or ""
  if not exporter.colors or exporter.colors == 0 then
    return jsString(value)
  end
  local c = code < 8 and tostring(code) or ("8;5;" .. code)
  return string.format("\27[3%s%sm%s\27[0m", c, exporter.colors >= 2 and decoration or "", jsString(value))
end

function Logger.code(name, level)
  local hash = 0
  for i = 1, #name do
    hash = ((hash << 3) - hash) + name:byte(i) + 13
    -- `hash |= 0`: wrap to a signed 32-bit integer
    hash = hash & 0xffffffff
    if hash >= 0x80000000 then
      hash = hash - 0x100000000
    end
  end
  local colors = (not level or level == 0) and {} or (level >= 2 and c256 or c16)
  if #colors == 0 then
    return nil
  end
  return colors[math.abs(hash) % #colors + 1]
end

function Logger.format(exporter, message)
  local args = table.move(message.args, 1, message.args.n or #message.args, 1, {})
  local n = message.args.n or #message.args
  if isError(args[1]) then
    args[1] = args[1].traceback and (tostring(args[1]) .. args[1].traceback) or args[1].message
    args[1] = tostring(args[1])
    table.insert(args, 1, "%s")
    n = n + 1
  elseif type(args[1]) ~= "string" then
    table.insert(args, 1, "%o")
    n = n + 1
  end

  local format = args[1]
  local i = 2
  format = format:gsub("%%([a-zA-Z%%])", function(char)
    if char == "%" then
      return "%"
    end
    local formatter = (exporter.formatters and exporter.formatters[char]) or defaultFormatters[char]
    if type(formatter) == "function" then
      local value = args[i]
      i = i + 1
      return tostring(formatter(value, exporter, message))
    end
    return "%" .. char
  end)

  local oFormatter = (exporter.formatters and exporter.formatters.o) or defaultFormatters.o
  for j = i, n do
    local arg = args[j]
    if type(arg) == "table" and arg ~= symbols.null then
      arg = oFormatter(arg, exporter, message)
    end
    format = format .. " " .. jsString(arg)
  end

  local maxLength = exporter.maxLength or 10240
  local lines = {}
  for raw in (format .. "\n"):gmatch("(.-)\r?\n") do
    local line = raw
    if #line > maxLength then
      line = line:sub(1, maxLength) .. "..."
    end
    lines[#lines + 1] = line
  end
  return table.concat(lines, "\n")
end

local function isAggregateError(e)
  return isError(e) and type(e.errors) == "table"
end

function Logger:constructor(options, service)
  for k, v in pairs(options) do
    self[k] = v
  end
  self.service = service
  self.error = self:_method("error", LoggerLevel.ERROR)
  self.info = self:_method("info", LoggerLevel.INFO)
  self.warn = self:_method("warn", LoggerLevel.WARN)
  self.debug = self:_method("debug", LoggerLevel.DEBUG)
end

-- logger methods are arrow functions upstream; they drop the receiver
function Logger:_method(type_, level)
  local this = self
  return function(_, ...)
    local args = table.pack(...)
    if args.n == 1 and isError(args[1]) then
      local e = args[1]
      if e.cause ~= nil then
        this[type_](this, e.cause)
      elseif isAggregateError(e) and #e.errors > 0 then
        -- An aggregate with no members has nothing to flatten; it is logged
        -- as itself, like any other error.
        for _, inner in ipairs(e.errors) do
          this[type_](this, inner)
        end
        return
      end
    end

    local service = this.service
    service._snMessage = service._snMessage + 1
    local sn = service._snMessage
    local host = service.ctx.host
    local ts = (host and host.now and host.now()) or (os and os.time and os.time() * 1000) or 0
    for _, entry in ipairs(service.exporters:entries()) do
      local id, exporter = entry[1], entry[2]
      local levels = exporter.levels
      local targetLevel = (levels and (levels[this.name] or levels.default)) or this.level or LoggerLevel.INFO
      if targetLevel >= level then
        local message = { sn = sn, ts = ts, type = type_, level = level, name = this.name, args = args }
        if this.meta then
          for k, v in pairs(this.meta) do
            message[k] = v
          end
        end
        local scope = service._exporterScopes[id]
        pcall(future.runSync, host, scope, exporter.export, message)
      end
    end
  end
end

-- `new WeakRef(value)`
local function weakRef(value)
  local box = setmetatable({ value = value }, { __mode = "v" })
  return {
    deref = function()
      return box.value
    end,
  }
end

local LoggerService = class("LoggerService")
M.LoggerService = LoggerService

function LoggerService:constructor(ctx)
  local tracker = { property = "ctx", noShadow = true }
  self.bufferSize = 1000
  self.buffer = {}
  self._snMessage = 0
  self._snExporter = 0
  self.exporters = Map.new()
  self._exporterScopes = {}
  self.ctx = ctx
  -- createCallable('logger', ...): the callable keeps the name `logger`
  self.name = "logger"
  rawset(self, S.tracker, tracker)

  local this = self
  self:exporter({
    colors = 3,
    export = function(message)
      local buffer = this.buffer
      buffer[#buffer + 1] = message
      -- for better performance
      local overflow = #buffer - this.bufferSize
      if overflow == 1 then
        table.remove(buffer, 1)
      elseif overflow > 1 then
        table.move(buffer, overflow + 1, #buffer + overflow, 1)
      end
    end,
  })
end

function LoggerService.__call(self, ...)
  local proxy = trace.createTraceable(proto.get(self, "ctx", self), self, proto.get(self, S.tracker, self))
  return proxy(...)
end

function LoggerService:exporter(exporter)
  local this = self
  local host = self.ctx.host
  local scope = future.capture(host)
  return self.ctx:effect(function()
    this._snExporter = this._snExporter + 1
    local id = this._snExporter
    this.exporters:set(id, exporter)
    if scope then
      this._exporterScopes[id] = scope
    end
    return future.internal(function()
      this._exporterScopes[id] = nil
      return this.exporters:delete(id)
    end)
  end, "ctx.logger.exporter()")
end

LoggerService[S.invoke] = function(self, name)
  local Service = require("cordis.service")
  local caller = self[S.caller]
  local config = Service[S.resolveIntercept](self)
  local fiber = (caller or self.ctx).fiber
  if name == nil then
    name = config.name
  end
  if name == nil then
    name = hyphenate(fiber.name)
  end
  return Logger.new({
    name = name,
    level = config.level,
    meta = { fiber = weakRef(fiber) },
  }, self)
end

for _, type_ in ipairs({ "error", "info", "warn", "debug" }) do
  LoggerService[type_] = function(self, ...)
    local logger = self()
    return logger[type_](logger, ...)
  end
end

return M
