-- Index of fibers by the service names they inject.
--
-- ReflectService:notify(names) walks every fiber of every runtime and checks
-- `has(fiber.inject, name)`; upstream does the same. This index lets it skip the
-- walk when no fiber injects any of the names, and skip each fiber that injects
-- none of them, so what `notify` does to the fibers (and in which order) is
-- unchanged. A fiber's `inject` is set in its constructor and by `_replace` only,
-- always a fresh table from `Inject.resolve`, and never changed afterwards.
local proto = require("cordis.proto")

local M = {}

local weak = { __mode = "k" }
-- name -> set of fibers whose inject has it
local byName = {}
-- fibers whose inject can't be indexed (not a proto object or a plain table):
-- always candidates
local always = setmetatable({}, weak)
-- fiber -> the names it is indexed under
local indexed = setmetatable({}, weak)
-- runtime -> { byName = { [name] = weakSet }, always = weakSet }
local byRuntime = setmetatable({}, weak)
-- fiber -> runtime under which it is currently indexed
local fiberRuntime = setmetatable({}, weak)

local function getRuntimeBucket(rt)
  local bucket = byRuntime[rt]
  if not bucket then
    bucket = {
      byName = {},
      always = setmetatable({}, weak),
    }
    byRuntime[rt] = bucket
  end
  return bucket
end

local function namesOf(inject)
  local out, seen = {}, {}
  local o = inject
  while o ~= nil do
    if proto.is(o) then
      for _, k in ipairs(proto.ownKeys(o)) do
        if not seen[k] then
          seen[k] = true
          out[#out + 1] = k
        end
      end
      o = proto.getPrototypeOf(o)
    elseif type(o) == "table" and getmetatable(o) == nil then
      for k in pairs(o) do
        if not seen[k] then
          seen[k] = true
          out[#out + 1] = k
        end
      end
      o = nil
    else
      return nil
    end
  end
  return out
end

function M.remove(fiber)
  always[fiber] = nil
  local rt = fiberRuntime[fiber]
  local rtBucket
  if rt then
    fiberRuntime[fiber] = nil
    rtBucket = byRuntime[rt]
    if rtBucket then
      rtBucket.always[fiber] = nil
    end
  end
  local names = indexed[fiber]
  if names then
    indexed[fiber] = nil
    for _, name in ipairs(names) do
      local set = byName[name]
      if set then
        set[fiber] = nil
        if next(set) == nil then
          byName[name] = nil
        end
      end
      if rtBucket then
        local rset = rtBucket.byName[name]
        if rset then
          rset[fiber] = nil
          if next(rset) == nil then
            rtBucket.byName[name] = nil
          end
        end
      end
    end
  end
  if rtBucket and next(rtBucket.always) == nil and next(rtBucket.byName) == nil then
    byRuntime[rt] = nil
  end
end

function M.set(fiber, inject)
  M.remove(fiber)
  if inject == nil then
    return
  end
  local rt = fiber.runtime
  local names = namesOf(inject)
  if not names then
    always[fiber] = true
    if rt then
      fiberRuntime[fiber] = rt
      getRuntimeBucket(rt).always[fiber] = true
    end
    return
  end
  if #names == 0 then
    return
  end
  indexed[fiber] = names
  local rtBucket
  if rt then
    fiberRuntime[fiber] = rt
    rtBucket = getRuntimeBucket(rt)
  end
  for _, name in ipairs(names) do
    local set = byName[name]
    if not set then
      set = setmetatable({}, weak)
      byName[name] = set
    end
    set[fiber] = true
    if rtBucket then
      local rset = rtBucket.byName[name]
      if not rset then
        rset = setmetatable({}, weak)
        rtBucket.byName[name] = rset
      end
      rset[fiber] = true
    end
  end
end

-- whether some fiber may inject one of `names`
function M.any(names)
  if next(always) ~= nil then
    return true
  end
  for _, name in ipairs(names) do
    local set = byName[name]
    if set and next(set) ~= nil then
      return true
    end
  end
  return false
end

-- whether some fiber in `runtime` may inject one of `names`
function M.anyIn(runtime, names)
  local rtBucket = byRuntime[runtime]
  if not rtBucket then
    return false
  end
  if next(rtBucket.always) ~= nil then
    return true
  end
  local rtByName = rtBucket.byName
  for _, name in ipairs(names) do
    local set = rtByName[name]
    if set and next(set) ~= nil then
      return true
    end
  end
  return false
end

-- whether `fiber` may inject one of `names`
function M.injects(fiber, names)
  if always[fiber] then
    return true
  end
  for _, name in ipairs(names) do
    local set = byName[name]
    if set and set[fiber] then
      return true
    end
  end
  return false
end

return M
