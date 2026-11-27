-- Ported from cordis packages/loader/src/config/tree.ts (MIT, (c) Shigma)
--
-- Deviations (docs/deviations.md, loader section): components are loaded
-- with `require`; `cordis.null` moves an entry to the root group;
-- positions count from 1; Futures; random ids.
local cordis = require("cordis")
local utils = require("cordis.loader.utils")
local proto = cordis.proto
local Future = cordis.Future

local M = {}

-- set by cordis.loader.init (circular modules)
local Entry, EntryGroup
function M._bind(entry, group)
  Entry, EntryGroup = entry, group
end

local function members(entry)
  local tree = entry.parent.tree
  local result = { entry }
  local group = entry.subgroup
  if not group or group.tree ~= tree then
    return result
  end
  for _, options in ipairs(group.data) do
    local member = tree.store[options.id]
    if member and member.parent == group then
      for _, m in ipairs(members(member)) do
        result[#result + 1] = m
      end
    end
  end
  return result
end

local ModuleNotFoundError = cordis.class("ModuleNotFoundError", cordis.Error)
M.ModuleNotFoundError = ModuleNotFoundError

function ModuleNotFoundError:constructor(specifier, message)
  cordis.Error.constructor(self, message)
  self.name = "ModuleNotFoundError"
  self.code = "ERR_MODULE_NOT_FOUND"
  self.specifier = specifier
end

-- `require` names the module in the first line:
-- `module '<name>' not found:` followed by one line per searcher
local function parseNotFound(err)
  if type(err) ~= "string" then
    return
  end
  local first, rest = err:match("^([^\n]*)\n?(.*)$")
  local name = first:match("^module '(.*)' not found:$")
  if not name then
    return
  end
  local hints = {}
  for line in rest:gmatch("[^\n]+") do
    hints[#hints + 1] = line
  end
  return { name = name, description = first:sub(1, -2), hints = hints }
end

local copy = utils.copy

---One tree-side mutation, reported to `EntryTree:commit()` synchronously after
---it has been applied to `entry.options` / `group.data` (see tree.ts:190-208):
---`{ id, group, from?, options?, legacy? }`.

local EntryTree = cordis.class("EntryTree")
M.EntryTree = EntryTree

EntryTree.sep = ":"

function EntryTree:constructor(ctx)
  self.ctx = ctx
  self.store = proto.create(nil)
  self.root = EntryGroup.new(self.ctx, self)
  local entry = self.ctx[Entry.key]
  if entry then
    entry.subtree = self
  end
end

EntryTree.__get.context = function(self)
  return self.ctx
end

function EntryTree:entries()
  local result = {}
  for _, id in ipairs(proto.ownKeys(self.store)) do
    local entry = self.store[id]
    result[#result + 1] = entry
    if entry.subtree then
      for _, e in ipairs(entry.subtree:entries()) do
        result[#result + 1] = e
      end
    end
  end
  return result
end

function EntryTree:getTasks()
  local tasks = {}
  for _, entry in ipairs(self:entries()) do
    if entry._initTask then
      tasks[#tasks + 1] = entry._initTask
    else
      if entry.outerFiber and entry.outerFiber.inertia then
        tasks[#tasks + 1] = entry.outerFiber.inertia
      end
      if entry.innerFiber and entry.innerFiber.inertia then
        tasks[#tasks + 1] = entry.innerFiber.inertia
      end
    end
  end
  return tasks
end

function EntryTree:await()
  local host = self.ctx.host
  return utils.spawn(host, function()
    -- an `async` function settles after the current job at the earliest
    cordis.tick(host)
    while true do
      local tasks = self:getTasks()
      if #tasks == 0 then
        return
      end
      Future.allSettled(host, tasks):await()
    end
  end)
end

function EntryTree:ensureId(options)
  if options.id == nil or options.id == "" then
    repeat
      options.id = string.format("%08x", math.random(0, 0xffffffff))
    until not self.store[options.id]
  end
  return options.id
end

local function split(id, sep)
  local parts, start = {}, 1
  while true do
    local i = id:find(sep, start, true)
    if not i then
      parts[#parts + 1] = id:sub(start)
      return parts
    end
    parts[#parts + 1] = id:sub(start, i - 1)
    start = i + #sep
  end
end

function EntryTree:resolve(id)
  local parts = split(id, EntryTree.sep)
  local tree = self
  local final = table.remove(parts)
  for _, part in ipairs(parts) do
    local entry = tree.store[part]
    tree = entry and entry.subtree
    if not tree then
      error(cordis.Error.new("cannot resolve entry " .. id), 2)
    end
  end
  local entry = tree.store[final]
  if not entry then
    error(cordis.Error.new("cannot resolve entry " .. id), 2)
  end
  return entry
end

function EntryTree:resolveGroup(id)
  if cordis.isNullable(id) or id == "" then
    return self.root
  end
  local entry = self:resolve(id)
  if not entry.subgroup then
    error(cordis.Error.new("entry " .. id .. " is not a group"), 2)
  end
  return entry.subgroup
end

-- `array.splice(position ?? Infinity, 0, item)` with 1-based positions
local function insert(list, item, position)
  if position == nil or position > #list + 1 then
    position = #list + 1
  elseif position < 1 then
    position = 1
  end
  table.insert(list, position, item)
end

-- `async create(options, parent = null, position = Infinity)`
function EntryTree:create(options, parent, position)
  return utils.try(self.ctx.host, function()
    local group = self:resolveGroup(parent)
    local issues = utils.validateOptions(options, EntryTree.sep, true)
    if #issues > 0 then
      error(utils.EntryValidationError.new(issues), 0)
    end
    if options.id ~= nil and options.id ~= "" and group.tree.store[options.id] then
      error(cordis.Error.new("entry " .. options.id .. " already exists"), 0)
    end
    local id = group.tree:ensureId(options)
    insert(group.data, options, position)
    group.tree:commit({ id = id, group = group, options = options })
    return group:create(options)
  end)
end

function EntryTree:remove(id)
  local entry = self:resolve(id)
  local group = entry.parent
  local legacy = entry.options
  group:remove(legacy.id)
  group.tree:commit({ id = legacy.id, group = group, legacy = legacy })
end

-- `async update(id, options, parent?, position?)`; `parent` nil: do not move,
-- `cordis.null`: move to the root group
function EntryTree:update(id, options, parent, position)
  return utils.try(self.ctx.host, function()
    local entry = self:resolve(id)
    local source = entry.parent
    local legacy = copy(entry.options)
    if type(options) == "table" then
      local newId = utils.getOwn(options, "id")
      if newId ~= nil and newId ~= entry.options.id then
        error(cordis.Error.new("cannot change entry id"), 0)
      end
    end
    local issues = utils.validateOptions(Entry.merge(copy(legacy), options), EntryTree.sep, true)
    if #issues > 0 then
      error(utils.EntryValidationError.new(issues), 0)
    end
    if parent ~= nil then
      local target = self:resolveGroup(parent)
      if entry.subgroup then
        local g = target
        while g do
          if rawequal(g, entry.subgroup) then
            error(cordis.Error.new("cannot move group into itself or its descendant"), 0)
          end
          local owner = g.ctx[Entry.key]
          g = owner and owner.parent or nil
        end
      end
      local transferred = target.tree == source.tree and {} or members(entry)
      for _, member in ipairs(transferred) do
        local mid = member.options.id
        if target.tree.store[mid] then
          error(cordis.Error.new("entry " .. mid .. " already exists in the target tree"), 0)
        end
      end
      source:unlink(entry.options)
      insert(target.data, entry.options, position)
      entry.parent = target
      -- The target tree owns the transferred entries from now on, so that it adopts
      -- them instead of creating new ones when it reconciles its source.
      for _, member in ipairs(transferred) do
        local mid = member.options.id
        proto.delete(source.tree.store, mid)
        target.tree.store[mid] = member
        if member.subgroup and member.subgroup.tree == source.tree then
          member.subgroup.tree = target.tree
        end
      end
    end
    -- `Entry:update` assigns the new options before it returns, so the change
    -- is fully visible to `commit()` here.
    local task = entry:update(options, false, true)
    if entry.parent.tree ~= source.tree then
      -- a transfer across trees removes the entry from the source tree and
      -- creates it in the target tree
      source.tree:commit({ id = legacy.id, group = source, legacy = legacy })
      entry.parent.tree:commit({ id = legacy.id, group = entry.parent, options = entry.options })
    else
      local from = entry.parent ~= source and source or nil
      entry.parent.tree:commit({
        id = legacy.id,
        group = entry.parent,
        from = from,
        options = entry.options,
        legacy = legacy,
      })
    end
    return task
  end)
end

-- Load a component module: through `loader.internal:import(name)` when
-- the host installed one, else `require(name)`. Returns a Future.
function EntryTree:import(name, getOuterStack)
  local host = self.ctx.host
  return utils.spawn(host, function()
    local loader = self.ctx.loader
    local internal = loader and loader.internal
    local ok, result
    if internal then
      ok, result = pcall(function()
        return cordis.await(internal:import(name))
      end)
    else
      ok, result = pcall(require, name)
    end
    if ok then
      return result
    end
    local notFound = parseNotFound(result)
    -- a module that the component itself requires is left to `require`
    if not notFound or notFound.name ~= name then
      error(result, 0)
    end
    local lines = { notFound.description }
    for _, hint in ipairs(notFound.hints) do
      lines[#lines + 1] = hint
    end
    local err = ModuleNotFoundError.new(name, table.concat(lines, "\n"))
    local frames = getOuterStack and getOuterStack() or {}
    if #frames > 0 then
      utils.relocate(err, err.message, frames)
    end
    error(err, 0)
  end)
end

function EntryTree:commit(_change)
  error(cordis.Error.new("EntryTree:commit() is abstract"), 2)
end

---Where a value of the entry options is written. A path that leaves the
---source falls back to its deepest written ancestor.
function EntryTree:getLocation(_id, _path)
  return nil
end

function EntryTree:locateExpr(_id, _path, _expr, _offset)
  return nil
end

return M
