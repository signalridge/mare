-- Minimal class convention: JS `class` / `new` / `instanceof`.
--
--   local Foo = class("Foo", Parent)
--   function Foo:constructor(...) end
--   Foo.__get.prop = function(self) end   -- getter (receives the receiver)
--   Foo.__set.prop = function(self, v) end
--   local foo = Foo.new(...)
--
-- An instance is a plain Lua table whose metatable is its class. Lookups that
-- miss the instance walk the class chain: getters first, then plain members.
local M = {}

local type = type
local rawget = rawget
local rawset = rawset
local getmetatable = getmetatable
local setmetatable = setmetatable

local WEAK_KEYS = { __mode = "k" }
local classes = setmetatable({}, WEAK_KEYS)
local classGetters = setmetatable({}, WEAK_KEYS)
local classHasGetters = setmetatable({}, WEAK_KEYS)
local classSetters = setmetatable({}, WEAK_KEYS)
local classHasSetters = setmetatable({}, WEAK_KEYS)
local classSuper = setmetatable({}, WEAK_KEYS)
local classChildren = setmetatable({}, WEAK_KEYS)
M._classes = classes
M._getters = classGetters
M._hasGetters = classHasGetters
M._super = classSuper

local METAMETHODS = {
  "__call",
  "__tostring",
  "__eq",
  "__lt",
  "__le",
  "__len",
  "__concat",
  "__unm",
  "__add",
  "__close",
}

local function isClass(x)
  return classes[x] == true or (type(x) == "table" and rawget(x, "__class") == true)
end
M.isClass = isClass

-- look up `k` along the class chain of `cls`, calling getters with `receiver`
local function lookup(cls, k, receiver)
  local c = cls
  while c do
    local getters = classHasGetters[c]
    if getters then
      local g = getters[k]
      if g then
        return g(receiver)
      end
    end
    local v = rawget(c, k)
    if v ~= nil then
      return v
    end
    c = classSuper[c]
  end
end
M.lookup = lookup

-- does the class chain define `k` (member, getter or setter)?
local function defines(cls, k)
  local c = cls
  while c do
    if rawget(c, k) ~= nil then
      return true
    end
    local getters = classHasGetters[c]
    if getters and getters[k] then
      return true
    end
    local setters = classHasSetters[c]
    if setters and setters[k] then
      return true
    end
    c = classSuper[c]
  end
  return false
end
M.defines = defines

local function findSetter(cls, k)
  local c = cls
  while c do
    local setters = classHasSetters[c]
    if setters then
      local s = setters[k]
      if s then
        return s
      end
    end
    local getters = classHasGetters[c]
    -- a getter without setter makes the property read-only
    if getters and getters[k] then
      return false
    end
    c = classSuper[c]
  end
end
M.findSetter = findSetter

local function classOf(x)
  local mt = getmetatable(x)
  if classes[mt] then
    return mt
  end
  return nil
end
M.classOf = classOf

local function new(cls, ...)
  local self = setmetatable({}, cls)
  local ctor = lookup(cls, "constructor", self)
  if ctor then
    local result = ctor(self, ...)
    if result ~= nil then
      return result
    end
  end
  return self
end

---Construct an instance of `cls` (upstream `new Class(...)`). The library constructs components
---through this, never through `cls.new`: `new` is an ordinary field, so a class may define a
---method named `new` (a service contract may require one), which then shadows the convenience
---`Class.new` but not construction.
M.construct = new

local function is(cls, x)
  if M.unwrap and type(x) == "table" and not classes[getmetatable(x)] then
    x = M.unwrap(x)
  end
  local c = classOf(x)
  while c do
    if c == cls then
      return true
    end
    c = classSuper[c]
  end
  return false
end

local function installNewindex(cls)
  if rawget(cls, "__newindex") ~= nil then
    return
  end
  local parent = classSuper[cls]
  if parent == nil then
    local getters = classGetters[cls]
    local setters = classSetters[cls]
    rawset(cls, "__newindex", function(self, k, v)
      local s = setters[k]
      if s then
        return s(self, v)
      end
      if getters[k] then
        error(string.format("cannot set read-only property %q", tostring(k)), 2)
      end
      rawset(self, k, v)
    end)
  else
    rawset(cls, "__newindex", function(self, k, v)
      local s = findSetter(cls, k)
      if s then
        return s(self, v)
      end
      if s == false then
        error(string.format("cannot set read-only property %q", tostring(k)), 2)
      end
      rawset(self, k, v)
    end)
  end
  local children = classChildren[cls]
  if children then
    for child in pairs(children) do
      installNewindex(child)
    end
  end
end

local function chainHasAccessors(cls)
  local c = cls
  while c do
    if classHasGetters[c] or classHasSetters[c] then
      return true
    end
    c = classSuper[c]
  end
  return false
end

local function chainHasGetters(cls)
  local c = cls
  while c do
    if classHasGetters[c] then
      return true
    end
    c = classSuper[c]
  end
  return false
end

local function installGetterIndex(cls)
  if type(rawget(cls, "__index")) == "function" then
    return
  end
  local parent = classSuper[cls]
  local getters = classGetters[cls]
  if parent == nil then
    rawset(cls, "__index", function(self, key)
      local g = getters[key]
      if g then
        return g(self)
      end
      return rawget(cls, key)
    end)
  else
    rawset(cls, "__index", function(self, k)
      local g = getters[k]
      if g then
        return g(self)
      end
      local v = rawget(cls, k)
      if v ~= nil then
        return v
      end
      return lookup(parent, k, self)
    end)
  end
  local children = classChildren[cls]
  if children then
    for child in pairs(children) do
      installGetterIndex(child)
    end
  end
end

function M.class(name, parent)
  local getters = {}
  local setters = {}
  local cls = {
    __class = true,
    __name = name,
    name = name,
    super = parent,
    __get = getters,
    __set = setters,
  }
  classes[cls] = true
  classGetters[cls] = getters
  classSetters[cls] = setters
  classSuper[cls] = parent
  if parent then
    local children = classChildren[parent]
    if not children then
      children = setmetatable({}, WEAK_KEYS)
      classChildren[parent] = children
    end
    children[cls] = true
  end
  -- Getter-free classes (including subclasses whose ancestor chain has no getters) use table
  -- `__index = cls` chained via `setmetatable(cls, { __index = parent })` so Lua resolves
  -- inherited methods in C with zero Lua metamethod calls; adding a getter anywhere on the
  -- chain upgrades `cls` and its subclasses to the getter-aware function `__index`.
  cls.__index = cls
  setmetatable(getters, {
    __newindex = function(t, k, v)
      rawset(t, k, v)
      if v ~= nil then
        if not classHasGetters[cls] then
          classHasGetters[cls] = getters
          installGetterIndex(cls)
        end
        installNewindex(cls)
      end
    end,
  })
  if parent and chainHasGetters(parent) then
    installGetterIndex(cls)
  end
  setmetatable(setters, {
    __newindex = function(t, k, v)
      rawset(t, k, v)
      if v ~= nil then
        if not classHasSetters[cls] then
          classHasSetters[cls] = setters
        end
        installNewindex(cls)
      end
    end,
  })
  if parent and chainHasAccessors(parent) then
    installNewindex(cls)
  end
  if parent then
    for _, mm in ipairs(METAMETHODS) do
      local f = rawget(parent, mm)
      if f ~= nil then
        cls[mm] = f
      end
    end
  end
  cls.new = function(...)
    return new(cls, ...)
  end
  cls.is = function(x)
    return is(cls, x)
  end
  -- statics and methods are inherited; metamethods of the class table itself
  -- are not those of its instances
  return setmetatable(cls, {
    __index = parent,
    __tostring = function()
      return "class " .. name
    end,
  })
end

return M
