-- Ported from cordis packages/core/src/fiber.ts (ValidationError, resolveConfig, `:14-58`) (MIT, (c) Shigma)
local errors = require("cordis.error")
local class = require("cordis.class").class
local symbols = require("cordis.symbols")

local kValidationError = symbols.symbolFor("ValidationError")

local ValidationError = class("ValidationError", errors.TypeError)
ValidationError[kValidationError] = true

function ValidationError.normalizePath(path)
  local r = {}
  for i, segment in ipairs(path or {}) do
    if type(segment) == "table" and not symbols.isSymbol(segment) then
      r[i] = segment.key
    else
      r[i] = segment
    end
  end
  return r
end

function ValidationError.formatPath(path)
  local r = {}
  for i, seg in ipairs(ValidationError.normalizePath(path)) do
    r[i] = tostring(seg)
  end
  return table.concat(r, ".")
end

function ValidationError:constructor(issues)
  local lines = {}
  for i, issue in ipairs(issues) do
    if issue.path and #issue.path > 0 then
      lines[i] = string.format("  - %s (at %s)", issue.message, ValidationError.formatPath(issue.path))
    else
      lines[i] = "  - " .. issue.message
    end
  end
  errors.TypeError.constructor(self, "invalid config:\n" .. table.concat(lines, "\n"))
  self.name = "ValidationError"
  self.issues = issues
end

function ValidationError.is(e)
  return type(e) == "table" and e[kValidationError] == true
end

local Future = require("cordis.future").Future

-- `resolveConfig(runtime, config)` (fiber.ts:46-58): Standard Schema validation
local function resolveConfig(runtime, config)
  if not runtime.Config then
    return config
  end
  local standard = runtime.Config["~standard"]
  local result = standard.validate(config)
  if Future.is(result) then
    error(errors.TypeError.new("Async config validation is not supported"))
  end
  if result.issues then
    error(ValidationError.new(result.issues))
  else
    return result.value
  end
end

return {
  ValidationError = ValidationError,
  resolveConfig = resolveConfig,
}
