-- Ported from cordis packages/loader/src/config/group.ts (MIT, (c) Shigma)
--
-- Deviations: symbols; sequences; Futures, `cordis.asyncIter`.
local cordis = require("cordis")
local utils = require("cordis.loader.utils")
local proto = cordis.proto
local Future = cordis.Future

local M = {}

-- set by cordis.loader.init (circular modules)
local Entry, EntryTree
function M._bind(entry, tree)
  Entry, EntryTree = entry, tree
end

local EntryGroup = cordis.class("EntryGroup")
M.EntryGroup = EntryGroup

EntryGroup.key = cordis.symbolFor("cordis.group")

function EntryGroup:constructor(ctx, tree)
  self.ctx = ctx
  self.tree = tree
  self.data = {}
  local entry = ctx[Entry.key]
  if entry then
    entry.subgroup = self
  end
end

EntryGroup.__get.context = function(self)
  return self.ctx
end

-- `async create(options)`; the prelude up to `entry.update()` runs now
function EntryGroup:create(options)
  return utils.try(self.ctx.host, function()
    local issues = utils.validateOptions(options, EntryTree.sep)
    if #issues > 0 then
      error(self:_reject(options, issues), 0)
    end
    local id = self.tree:ensureId(options)
    local entry = self.tree.store[id]
    if not entry then
      entry = Entry.new(self.ctx.loader)
      self.tree.store[id] = entry
    end
    -- Entry may be transferred from another group,
    -- so we need to update the parent reference.
    entry.parent = self
    -- Use `create = true` to replace existing entry.options.
    return entry:update(options, true, true):next(function()
      return entry.id
    end)
  end)
end

function EntryGroup:getOuterStack(id)
  local location = self.tree:getLocation(id, {})
  local frames = {}
  if location then
    frames[1] = "    at " .. id .. " (" .. utils.formatLocation(location) .. ")"
  end
  local entry = self.ctx[Entry.key]
  if entry then
    for _, frame in ipairs(entry.getOuterStack()) do
      frames[#frames + 1] = frame
    end
  end
  return frames
end

function EntryGroup:_reject(options, issues)
  local err = utils.EntryValidationError.new(issues)
  local id = type(options) == "table" and options.id or nil
  if cordis.isNullable(id) then
    return err
  end
  -- unregistered first, so the disposal is not taken for a self-dispose
  local entry = self.tree.store[id]
  if entry then
    proto.delete(self.tree.store, id)
    entry:dispose()
  end
  local frames = self:getOuterStack(id)
  if #frames == 0 then
    return err
  end
  local function locate(path)
    return self.tree:getLocation(id, path)
  end
  utils.relocate(err, utils.formatIssues("invalid entry options", issues, locate), frames)
  return err
end

function EntryGroup:unlink(options)
  local config = self.data
  for i, item in ipairs(config) do
    if rawequal(item, options) then
      table.remove(config, i)
      return
    end
  end
end

function EntryGroup:remove(id, isDispose)
  local entry = self.tree.store[id]
  -- an entry that another group already adopted (a file-driven transfer) belongs
  -- to that group now
  if not entry or entry.parent ~= self then
    return
  end
  -- Unregister before disposing: the `dispose` callback of the entry fiber
  -- distinguishes "removed by the loader" from "disposed itself" by checking
  -- whether the entry is still in the store.
  proto.delete(self.tree.store, id)
  entry:dispose()
  if not isDispose then
    self:unlink(entry.options)
  end
end

-- `async update(config)`
function EntryGroup:update(config)
  local host = self.ctx.host
  local oldConfig = self.data
  self.data = config
  -- ids in `Reflect.ownKeys({ ...oldMap, ...newMap })` order
  local ids, oldMap, newMap = {}, {}, {}
  local function add(id)
    if oldMap[id] == nil and newMap[id] == nil then
      ids[#ids + 1] = id
    end
  end
  for _, options in ipairs(oldConfig) do
    local id = type(options) == "table" and options.id or nil
    if id ~= nil then
      add(id)
      oldMap[id] = options
    end
  end
  for _, options in ipairs(config) do
    -- non-objects are rejected by `create`
    local id = type(options) == "table" and options.id or nil
    if id == nil then
      id = cordis.symbol("anonymous")
    end
    add(id)
    newMap[id] = options
  end

  -- update inner components
  local tasks = {}
  for _, id in ipairs(ids) do
    if newMap[id] ~= nil then
      tasks[#tasks + 1] = self:create(newMap[id]):catch(function(err)
        self.ctx.logger:error(err)
      end)
    else
      self:remove(id)
    end
  end
  return Future.all(host, tasks):next(function() end)
end

function EntryGroup:stop()
  for _, options in ipairs(self.data) do
    self:remove(options.id, true)
  end
end

local Group = cordis.class("Group", EntryGroup)
M.Group = Group

Group.initial = {}
Group[EntryGroup.key] = true

function Group:constructor(ctx, config)
  EntryGroup.constructor(self, ctx, ctx[Entry.key].parent.tree)
  self.config = config
  -- The entry list is shared with the group from the start, so entries that
  -- the tree inserts before `init` has run are not lost when `init` reconciles
  -- the list (upstream starts from `[]` and relies on microtask order).
  self.data = config
  ctx:on("internal/update", function(_, config)
    return self:update(config)
  end)
end

Group[cordis.symbols.init] = function(self)
  return cordis.asyncIter(function(yield)
    yield(function()
      self:stop()
    end)
    self:update(self.config):await()
  end)()
end

return M
