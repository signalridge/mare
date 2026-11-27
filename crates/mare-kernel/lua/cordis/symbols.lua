-- Ported from cordis packages/core/src/utils.ts (symbols, `:60-85`) (MIT, (c) Shigma)
--
-- Symbols: unique tables with a marker metatable.
local M = {}

local Symbol = { __name = "cordis.Symbol" }
Symbol.__index = Symbol
M._Symbol = Symbol
function Symbol.__tostring(self)
  return "Symbol(" .. (self.description or "") .. ")"
end

local registry = {}

---`Symbol(description)`: a fresh unique symbol.
function M.symbol(description)
  return setmetatable({ description = description }, Symbol)
end

---`Symbol.for(key)`: the symbol registered under `key` in this Lua state.
function M.symbolFor(key)
  local s = registry[key]
  if not s then
    s = setmetatable({ description = key, key = key }, Symbol)
    registry[key] = s
  end
  return s
end

function M.isSymbol(x)
  return getmetatable(x) == Symbol
end

local S = M.symbolFor
M.symbols = {
  -- internal symbols
  shadow = S("cordis.shadow"),
  caller = S("cordis.caller"),
  receiver = S("cordis.receiver"),
  original = S("cordis.original"),
  decorator = S("cordis.decorator"),
  initHooks = S("cordis.initHooks"),
  checkProto = S("cordis.checkProto"),

  -- context symbols
  root = S("cordis.root"),
  effect = S("cordis.effect"),
  filter = S("cordis.filter"),
  isolate = S("cordis.isolate"),
  intercept = S("cordis.intercept"),

  -- service symbols
  init = S("cordis.init"),
  check = S("cordis.check"),
  invoke = S("cordis.invoke"),
  metadata = S("cordis.metadata"),
  extend = S("cordis.extend"),
  tracker = S("cordis.tracker"),
  mergeIntercept = S("cordis.mergeIntercept"),
  resolveIntercept = S("cordis.resolveIntercept"),
}

-- `cordis.null`: the stored counterpart of JS `null`
M.null = setmetatable({}, {
  __name = "cordis.null",
  __tostring = function()
    return "null"
  end,
})

function M.isNullable(v)
  return v == nil or v == M.null
end

return M
