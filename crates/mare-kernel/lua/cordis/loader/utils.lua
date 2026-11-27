-- Ported from cordis packages/loader/src/config/utils.ts (MIT, (c) Shigma)
--
-- Deviations (docs/deviations.md, loader section): expressions are Lua
-- (`{ __luaExpr = "..." }`); evaluation errors are traced by line only;
-- sequences and issue paths count from 1, an empty table is both an
-- array and a plain object; error tables.
local cordis = require("cordis")
-- `errors.traceback` degrades to the bare message without the `debug` library
local errors = require("cordis.error")
local proto = cordis.proto
local class = cordis.class

local M = {}

-- frames below the wrapper belong to the caller
local WRAPPER = "cordis[/\\]loader[/\\]utils%.lua"

local counter = 0

---@class cordis.loader.LuaExpr
---@field __luaExpr string

function M.isLuaExpr(value)
  return type(value) == "table" and not proto.is(value) and type(rawget(value, "__luaExpr")) == "string"
end

local ReferenceError = class("ReferenceError", cordis.Error)
M.ReferenceError = ReferenceError

function ReferenceError:constructor(message)
  cordis.Error.constructor(self, message)
  self.name = "ReferenceError"
end

-- `with (ctx) { ... }`: names resolve on `ctx` first, then on the globals; a
-- name bound in neither raises, as an unbound identifier does in JS
local function scope(ctx)
  return setmetatable({}, {
    __index = function(_, k)
      if proto.has(ctx, k) then
        return ctx[k]
      end
      local value = _G[k]
      if value == nil then
        error(ReferenceError.new(tostring(k) .. " is not defined"), 2)
      end
      return value
    end,
    __newindex = function(_, k, v)
      if proto.has(ctx, k) then
        ctx[k] = v
      else
        _G[k] = v
      end
    end,
  })
end

local function compile(ctx, expr, chunkname)
  local fn, err = load("return " .. expr, "=" .. chunkname, "t", scope(ctx))
  if not fn then
    return nil, err
  end
  return fn
end

-- `SourceLocation`: 1-based; columns in bytes
function M.formatLocation(location)
  return string.format("%s:%d:%d", location.filename, location.line, location.column)
end

local formatLocation = M.formatLocation

-- the byte offset (0-based) of `line` in `source`, at its first column
local function toOffset(source, line)
  local start = 0
  for _ = 2, line do
    local i = source:find("\n", start + 1, true)
    if not i then
      return nil
    end
    start = i
  end
  return start
end

local function escape(s)
  return (s:gsub("[%^%$%(%)%%%.%[%]%*%+%-%?]", "%%%0"))
end

-- Rewrite the traceback of `error` so that frames inside the expression point
-- at its source and the outer stack of the entry follows (utils.ts:64-96).
local function traceEvaluation(err, traceback, expr, marker, trace)
  if not cordis.Error.like(err) then
    return err
  end
  local outer = trace.getOuterStack()
  local start = trace.locate(expr, 0)
  if #outer == 0 and not start then
    return err
  end
  local frames, own = {}, false
  for line in (traceback or ""):gmatch("[^\n]+") do
    local file, row = line:match("^%s+(.-):(%d+): in ")
    if line:find(WRAPPER) or line:find("in function 'xpcall'", 1, true) then
      break
    end
    if file == marker then
      own = true
      local offset = toOffset(expr, tonumber(row))
      local location = offset and trace.locate(expr, offset)
      local where = location and formatLocation(location) or (marker .. ":" .. row)
      frames[#frames + 1] = "    at " .. trace.label .. " (" .. where .. ")"
    elseif file then
      frames[#frames + 1] = "    at " .. line:match("^%s+(.*)$")
    end
  end
  -- a syntax error has no frame of its own
  if not own then
    frames[#frames + 1] = start and ("    at " .. trace.label .. " (" .. formatLocation(start) .. ")")
      or ("    at " .. trace.label)
  end
  local message = tostring(err.message):gsub("^" .. escape(marker) .. ":%d+: ", "")
  err.message = message
  local lines = { string.format("%s: %s", err.name or "Error", message) }
  table.move(frames, 1, #frames, #lines + 1, lines)
  table.move(outer, 1, #outer, #lines + 1, lines)
  err.traceback = table.concat(lines, "\n")
  return err
end

function M.evaluate(ctx, expr, trace)
  if not trace then
    counter = counter + 1
    local fn, err = compile(ctx, expr, "<cordis:expression#" .. counter .. ">")
    if not fn then
      error(err, 0)
    end
    return (fn())
  end
  -- unique, to tell nested evaluations apart
  counter = counter + 1
  local marker = "<cordis:expression#" .. counter .. ">"
  local fn, err = compile(ctx, expr, marker)
  local traceback
  if fn then
    local ok, result = xpcall(fn, function(e)
      traceback = errors.traceback("", 2)
      return e
    end)
    if ok then
      return result
    end
    err = result
  end
  -- error values that are strings cannot carry a traceback
  if type(err) == "string" then
    err = cordis.Error.new(err)
  end
  error(traceEvaluation(err, traceback, expr, marker, trace), 0)
end

-- own keys of an options table: a JS-like object by its own keys, a plain
-- table by `next`
function M.ownKeys(t)
  if proto.is(t) then
    return proto.ownKeys(t)
  end
  local keys = {}
  for k in next, t do
    keys[#keys + 1] = k
  end
  return keys
end

function M.getOwn(t, k)
  if proto.is(t) then
    return proto.getOwn(t, k)
  end
  return rawget(t, k)
end

-- `{ ...t }`: a plain table of the own fields of `t`
function M.copy(t)
  local result = {}
  for _, k in ipairs(M.ownKeys(t)) do
    result[k] = M.getOwn(t, k)
  end
  return result
end

-- a sequence (keys exactly 1..#t); the empty table counts
function M.isArray(value)
  if type(value) ~= "table" or getmetatable(value) ~= nil then
    return false
  end
  -- every position of 1..n is present: `#` may report any border of a
  -- table with holes, so the keys are counted as well
  local n, count = #value, 0
  for k in next, value do
    if math.type(k) ~= "integer" or k < 1 or k > n then
      return false
    end
    count = count + 1
  end
  return count == n
end

-- a plain object: neither an array (unless empty) nor an expression
function M.isPlainObject(value)
  if type(value) ~= "table" or M.isLuaExpr(value) then
    return false
  end
  if proto.is(value) then
    return true
  end
  if getmetatable(value) ~= nil then
    return false
  end
  return next(value) == nil or not M.isArray(value)
end

local function append(path, key)
  local result = table.move(path, 1, #path, 1, {})
  result[#result + 1] = key
  return result
end

local function sortedKeys(t)
  local keys = {}
  for k in next, t do
    keys[#keys + 1] = k
  end
  table.sort(keys, function(a, b)
    local ta, tb = type(a), type(b)
    if ta ~= tb then
      return ta < tb
    end
    if ta == "number" or ta == "string" then
      return a < b
    end
    return tostring(a) < tostring(b)
  end)
  return keys
end

function M.interpolate(ctx, value, getTrace, path, visiting)
  path = path or {}
  visiting = visiting or {}
  if M.isLuaExpr(value) then
    return M.evaluate(ctx, value.__luaExpr, getTrace and getTrace(path))
  elseif type(value) ~= "table" or value == cordis.null or cordis.isSymbol(value) then
    return value
  elseif proto.is(value) then
    local result = proto.create(nil)
    for _, key in ipairs(proto.ownKeys(value)) do
      result[key] = M.interpolate(ctx, proto.getOwn(value, key), getTrace, append(path, key), visiting)
    end
    return result
  elseif getmetatable(value) ~= nil then
    -- a table with a metatable (defaults through `__index`, a class instance):
    -- its own fields are interpolated like an object's (utils.ts:41); it is
    -- copied, keeping the metatable, only if something was evaluated
    -- a protected metatable (`__metatable`): getmetatable returns that field instead, so the
    -- table cannot be copied with it; it is kept as an opaque value
    local mt = getmetatable(value)
    if type(mt) ~= "table" or rawget(mt, "__metatable") ~= nil then
      return value
    end
    -- such tables may refer back to themselves; a cycle is left as it is
    if visiting[value] then
      return value
    end
    visiting[value] = true
    local result, changed = {}, false
    local ok, err = pcall(function()
      for _, key in ipairs(sortedKeys(value)) do
        local item = rawget(value, key)
        local v = M.interpolate(ctx, item, getTrace, append(path, key), visiting)
        result[key] = v
        if not rawequal(v, item) then
          changed = true
        end
      end
    end)
    visiting[value] = nil
    if not ok then
      error(err, 0)
    end
    if not changed then
      return value
    end
    return setmetatable(result, mt)
  elseif M.isArray(value) then
    local result = {}
    for index, item in ipairs(value) do
      result[index] = M.interpolate(ctx, item, getTrace, append(path, index), visiting)
    end
    return result
  else
    local result = {}
    for _, key in ipairs(sortedKeys(value)) do
      local item = value[key]
      result[key] = M.interpolate(ctx, item, getTrace, append(path, key), visiting)
    end
    return result
  end
end

-- replace the own keys of `target` with those of `source`
function M.swap(target, source)
  for _, key in ipairs(proto.ownKeys(target)) do
    proto.delete(target, key)
  end
  if source == nil or source == cordis.null then
    return
  end
  for _, key in ipairs(proto.ownKeys(source)) do
    proto.defineOwn(target, key, proto.getOwn(source, key))
  end
end

function M.formatIssue(message, location, path)
  local where = {}
  if path then
    where[#where + 1] = path
  end
  if location then
    where[#where + 1] = formatLocation(location)
  end
  if #where > 0 then
    return string.format("  - %s (at %s)", message, table.concat(where, ", "))
  end
  return "  - " .. message
end

local function joinPath(path)
  local parts = {}
  for i, key in ipairs(path) do
    parts[i] = tostring(key)
  end
  return table.concat(parts, ".")
end

function M.formatIssues(title, issues, locate)
  local lines = {}
  for i, issue in ipairs(issues) do
    lines[i] = M.formatIssue(
      issue.message,
      locate and locate(issue.path) or nil,
      #issue.path > 0 and joinPath(issue.path) or nil
    )
  end
  return title .. ":\n" .. table.concat(lines, "\n")
end

function M.relocate(err, message, frames)
  err.message = message
  local lines = { string.format("%s: %s", err.name or "Error", message) }
  table.move(frames, 1, #frames, 2, lines)
  err.traceback = table.concat(lines, "\n")
end

local EntryValidationError = class("EntryValidationError", cordis.TypeError)
M.EntryValidationError = EntryValidationError

function EntryValidationError:constructor(issues)
  cordis.TypeError.constructor(self, M.formatIssues("invalid entry options", issues))
  self.name = "EntryValidationError"
  self.issues = issues
end

local function isString(value)
  return type(value) == "string"
end

local function isBoolean(value)
  return type(value) == "boolean"
end

function M.validateOptions(options, sep, deep)
  if not M.isPlainObject(options) then
    return { { message = "entry must be an object", path = {} } }
  end
  local issues = {}
  local function check(key, expected, valid, required)
    local value = options[key]
    if cordis.isNullable(value) then
      if required then
        issues[#issues + 1] = { message = key .. " is required", path = { key } }
      end
    elseif not valid(value) then
      issues[#issues + 1] = { message = key .. " must be " .. expected, path = { key } }
    end
  end

  check("id", "a string", isString)
  if type(options.id) == "string" and options.id:find(sep, 1, true) then
    issues[#issues + 1] = { message = 'id must not contain "' .. sep .. '"', path = { "id" } }
  end
  check("name", "a string", isString, true)
  check("group", "a boolean", isBoolean)
  check("disabled", "a boolean", function(value)
    return type(value) == "boolean" or M.isLuaExpr(value)
  end)
  -- a group's config is its entry list
  if options.group == true then
    check("config", "an array", M.isArray, true)
  end

  check("inject", "an array or an object", function(value)
    return M.isArray(value) or M.isPlainObject(value)
  end)
  if M.isArray(options.inject) then
    for index, name in ipairs(options.inject) do
      if type(name) ~= "string" then
        issues[#issues + 1] = { message = "inject." .. index .. " must be a string", path = { "inject", index } }
      end
    end
  end
  check("intercept", "an object", M.isPlainObject)
  check("isolate", "an object", M.isPlainObject)
  if M.isPlainObject(options.isolate) then
    local keys = proto.is(options.isolate) and proto.ownKeys(options.isolate) or {}
    if not proto.is(options.isolate) then
      for name in pairs(options.isolate) do
        keys[#keys + 1] = name
      end
      table.sort(keys, function(a, b)
        return tostring(a) < tostring(b)
      end)
    end
    for _, name in ipairs(keys) do
      local label = options.isolate[name]
      if label ~= true and type(label) ~= "string" then
        issues[#issues + 1] =
          { message = "isolate." .. tostring(name) .. " must be true or a string", path = { "isolate", name } }
      end
    end
  end
  -- from a source, each entry is checked as its group creates it
  if deep and options.group == true and M.isArray(options.config) then
    for index, child in ipairs(options.config) do
      for _, issue in ipairs(M.validateOptions(child, sep, true)) do
        local path = { "config", index }
        table.move(issue.path, 1, #issue.path, 3, path)
        issues[#issues + 1] = { message = issue.message, path = path }
      end
    end
  end
  return issues
end

-- async helpers  ------------------------------------------------------------
--
-- An upstream `async` function runs synchronously up to its first `await`;
-- the loader relies on that (e.g. `Entry.update` assigns the options before it
-- returns). Ported async functions therefore run that prelude in the caller
-- and hand only the rest to a host task (`M.spawn`) or to Future chaining.

local future = require("cordis.future")
local Future = cordis.Future

---Run `fn(...)` in a new host task; returns a Future of its result.
function M.spawn(host, fn, ...)
  return future.AsyncFunction.spawn(future.async(fn), host, ...)
end

---Call `fn(...)` now; a raised error becomes a rejected Future, a plain result
---a fulfilled one (an `async` function that failed before its first `await`).
function M.try(host, fn, ...)
  local ok, result = pcall(fn, ...)
  if not ok then
    return Future.reject(host, result)
  end
  return Future.resolve(host, result)
end

---Strict deep equality of option values (cosmokit `deepEqual`, strict mode):
---plain tables and JS-like objects compare by own keys, anything else by identity.
function M.deepEqual(a, b)
  if rawequal(a, b) then
    return true
  end
  if type(a) ~= "table" or type(b) ~= "table" then
    return false
  end
  local pa, pb = proto.is(a), proto.is(b)
  if (getmetatable(a) ~= nil and not pa) or (getmetatable(b) ~= nil and not pb) then
    return false
  end
  local function keys(t)
    return proto.is(t) and proto.ownKeys(t)
      or (function()
        local r = {}
        for k in next, t do
          r[#r + 1] = k
        end
        return r
      end)()
  end
  local function own(t, k)
    if proto.is(t) then
      return proto.getOwn(t, k)
    end
    return rawget(t, k)
  end
  local ka, kb = keys(a), keys(b)
  if #ka ~= #kb then
    return false
  end
  for _, k in ipairs(ka) do
    local vb = own(b, k)
    if vb == nil or not M.deepEqual(own(a, k), vb) then
      return false
    end
  end
  return true
end

return M
