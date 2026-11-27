-- Error objects: `{ name, message, traceback, ... }` tables.
local class = require("cordis.class").class

-- `debug.traceback` when the `debug` library exists; sandboxed embedders
-- plugin environment have none, and tracebacks then degrade to the message
local function traceback(message, level)
  local dbg = rawget(_G, "debug")
  if type(dbg) == "table" and type(dbg.traceback) == "function" then
    return dbg.traceback(message, (level or 1) + 1)
  end
  return message or ""
end

local Error = class("Error")

function Error:constructor(message, options)
  self.name = self.name or "Error"
  self.message = message == nil and "" or tostring(message)
  self.traceback = traceback("", 3)
  if options and options.cause ~= nil then
    self.cause = options.cause
  end
end

function Error.__tostring(self)
  local name = self.name or "Error"
  if self.message == nil or self.message == "" then
    return name
  end
  return name .. ": " .. self.message
end

---true for any error-like table (has a string `message` field)
function Error.like(x)
  return type(x) == "table" and type(rawget(x, "message")) == "string"
end

---The message of any raised value.
function Error.messageOf(e)
  if type(e) == "table" and e.message ~= nil then
    return tostring(e.message)
  end
  return tostring(e)
end

local TypeError = class("TypeError", Error)
function TypeError:constructor(message, options)
  self.name = "TypeError"
  Error.constructor(self, message, options)
end

-- `AggregateError(errors, message)`
local AggregateError = class("AggregateError", Error)
function AggregateError:constructor(errors, message)
  self.name = "AggregateError"
  Error.constructor(self, message)
  self.errors = errors
end

local CordisError = class("CordisError", Error)
CordisError.Code = {
  INACTIVE_EFFECT = "cannot create effect on inactive context",
}
function CordisError:constructor(code, message)
  self.name = "CordisError"
  Error.constructor(self, message or CordisError.Code[code])
  self.code = code
end

return {
  traceback = traceback,
  Error = Error,
  TypeError = TypeError,
  AggregateError = AggregateError,
  CordisError = CordisError,
}
