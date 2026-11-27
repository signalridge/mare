-- Ported from cordis packages/loader/src/config/entry.ts (MIT, (c) Shigma)
--
-- Deviations (docs/deviations.md, loader section): Lua expressions;
-- `cordis.null` deletes an option; no key sorting; the part of
-- each upstream `async` method before its first `await` runs in the caller,
-- the rest in a host task or a Future chain; getters through `__get`.
local cordis = require("cordis")
local classMod = require("cordis.class")
local utils = require("cordis.loader.utils")
local isolate = require("cordis.loader.isolate")
local treeModule = require("cordis.loader.tree")
local proto = cordis.proto
local Future = cordis.Future

local M = {}

-- set by cordis.loader.init (circular modules)
local EntryGroup
function M._bind(group)
  EntryGroup = group
end

local copy = utils.copy

local function isSubclassOf(cls, base)
  local c = cls
  while c do
    if rawequal(c, base) then
      return true
    end
    c = classMod._super[c]
  end
  return false
end

-- JS `!!value` with Lua truthiness
local function truthy(v)
  return v ~= nil and v ~= false and v ~= cordis.null
end

local Entry = cordis.class("Entry")
M.Entry = Entry

Entry.key = cordis.symbolFor("cordis.entry")

-- holds the entry's `inject` and creates the inner fiber as its child
local outer = { name = "loader:outer" }
function outer.apply(ctx)
  return cordis.asyncIter(function(yield)
    local entry = ctx[Entry.key]
    -- re-imported so that a module reloaded by HMR in between is used
    local exports = entry:_import():await()
    -- stops here if a dependency was lost during the import; the verdict was
    -- evaluated with the services of this fiber, so it expires with them
    yield(function()
      if entry.outerFiber and entry.outerFiber.ctx.fiber == ctx.fiber then
        entry._verdict = nil
      end
    end)
    if exports then
      entry:_start(ctx, exports.component)
    end
  end)()
end
M.outer = outer

function Entry.isOuter(fiber)
  return fiber.runtime ~= nil and fiber.runtime.callback == outer.apply
end

function Entry:constructor(loader)
  local Context = cordis.Context
  self.loader = loader
  self.ctx = loader.ctx:extend({ [Entry.key] = self })
  self.ctx[Context.isolate] = proto.create(self.ctx[Context.isolate])
  -- safety: call `entry:update()` immediately after creating an entry
  self.options = {}
  self.inject = proto.create(nil)
  self._disposing = false
  self._intercept = proto.create(nil)

  self.getOuterStack = function()
    return self.parent:getOuterStack(self.options.id)
  end

  self.trace = function(path)
    local parts = {}
    for i, key in ipairs(path) do
      parts[i] = tostring(key)
    end
    return {
      label = table.concat(parts, "."),
      locate = function(expr, offset)
        return self:locateExpr(path, expr, offset)
      end,
      getOuterStack = self.getOuterStack,
    }
  end
end

function Entry.merge(target, options)
  for _, key in ipairs(utils.ownKeys(options)) do
    local value = utils.getOwn(options, key)
    if value == cordis.null then
      if proto.is(target) then
        proto.delete(target, key)
      else
        target[key] = nil
      end
    else
      target[key] = value
    end
  end
  return target
end

Entry.__get.context = function(self)
  return self.ctx
end

Entry.__get.id = function(self)
  local owner = self.parent.tree.ctx[Entry.key]
  return owner and (owner.id .. treeModule.EntryTree.sep .. self.options.id) or self.options.id
end

-- nil while the outer fiber is pending
function Entry:_evaluationContext()
  if not self.outerFiber then
    return self.ctx
  end
  if self.outerFiber.committed then
    return self.outerFiber.ctx
  end
end

function Entry:_resolveDisabled(ctx)
  local disabled = self.options.disabled
  local ok, result = pcall(function()
    if utils.isLuaExpr(disabled) then
      return truthy(utils.evaluate(ctx, disabled.__luaExpr, self.trace({ "disabled" })))
    end
    return truthy(disabled)
  end)
  if ok then
    self._verdict = result
  else
    self.ctx.logger:error(result)
    self._verdict = true
  end
end

Entry.__get.disabled = function(self)
  -- group is always enabled
  if self.options.group then
    return false
  end
  return not self:_enabled() or self:_parentDisabled()
end

-- Whether the verdict enables the entry. A verdict evaluated with the services
-- of the outer fiber does not hold once the fiber starts to unload, e.g. on
-- a dependency loss; the verdict itself is cleared when the unload finishes.
function Entry:_enabled()
  return self._verdict == false and (not self.outerFiber or self.outerFiber.state ~= cordis.FiberState.UNLOADING)
end

function Entry:_parentDisabled()
  local entry = self.parent.ctx[Entry.key]
  while entry do
    if not entry:_enabled() then
      return true
    end
    entry = entry.parent.ctx[Entry.key]
  end
  return false
end

function Entry:_blocked()
  if self.options.group then
    return false
  end
  -- an expression waits for the patched context
  if not utils.isLuaExpr(self.options.disabled) and truthy(self.options.disabled) then
    self._verdict = true
    return true
  end
  return self:_parentDisabled()
end

function Entry:evaluate(expr)
  local ctx = self:_evaluationContext()
  if not ctx then
    error(cordis.Error.new("entry " .. self.id .. " is waiting for its dependencies"), 2)
  end
  return utils.evaluate(ctx, expr)
end

local function isEmptyPlain(o)
  local s = proto._state[o]
  return s ~= nil and s.handler == nil and s.k1 == nil and (s.own == nil or s.own._size == 0)
end

function Entry:_hasOuter()
  local s = proto._state[self.inject]
  if s and s.handler == nil then
    return s.k1 ~= nil or (s.own ~= nil and s.own._size > 0)
  end
  return #proto.ownKeys(self.inject) > 0
end

-- returns whether `inject` changed, or nil on failure
function Entry:_resolveInject()
  local raw = self.options.inject
  if raw == nil or raw == cordis.null then
    if isEmptyPlain(self.inject) then
      return false
    end
    self.inject = proto.create(nil)
    return true
  end
  local ok, inject = pcall(function()
    local inject = cordis.Inject.resolve(raw)
    for _, key in ipairs(proto.ownKeys(inject)) do
      local value = utils.interpolate(self.parent.ctx, inject[key], self.trace, { "inject", key })
      -- an empty value keeps the dependency, without config (registry.ts:43);
      -- assigning nil would delete the key
      if value == nil then
        value = cordis.null
      end
      inject[key] = value
    end
    return inject
  end)
  if not ok then
    self.ctx.logger:error(inject)
    self.inject = proto.create(nil)
    self._verdict = true
    return nil
  end
  -- strict: `cordis.null` and a missing key differ
  local changed = not utils.deepEqual(inject, self.inject)
  self.inject = inject
  return changed
end

function Entry:_applyIntercept(ctx)
  local raw = self.options.intercept
  if raw == nil and isEmptyPlain(self._intercept) then
    return true
  end
  local ok, err = pcall(function()
    utils.swap(self._intercept, utils.interpolate(ctx, raw, self.trace, { "intercept" }))
  end)
  if ok then
    return true
  end
  self.ctx.logger:error(err)
  -- the component does not run with an intercept that cannot be evaluated
  self._verdict = true
  return false
end

-- `async _patchContext(diff)`
function Entry:_patchContext(diff)
  local Context = cordis.Context
  return utils.try(self.ctx.host, isolate.patchIsolate, self, function()
    proto.setPrototypeOf(self.ctx, self.parent.ctx)
    proto.setPrototypeOf(self._intercept, self.parent.ctx[Context.intercept])
    if self._holder then
      self:_hold(self._holder.fiber)
    end

    if self.outerFiber then
      if self.outerFiber.state ~= cordis.FiberState.ACTIVE then
        return
      end
      return self:_reconcile(self.outerFiber.ctx, diff)
    elseif self.innerFiber and self.innerFiber.uid then
      return self:_reconcile(self.ctx, diff)
    end
  end)
end

local function includes(list, value)
  for _, v in ipairs(list) do
    if v == value then
      return true
    end
  end
  return false
end

-- `async _reconcile(ctx, diff)`
function Entry:_reconcile(ctx, diff)
  return utils.try(self.ctx.host, function()
    if not (self.innerFiber and self.innerFiber.uid) then
      -- the reactivation evaluates `disabled`
      return self.outerFiber and self.outerFiber:restart()
    end
    self:_resolveDisabled(ctx)
    if self.disabled or not self:_applyIntercept(ctx) then
      self:_dispose(self.innerFiber)
      return
    end
    if includes(diff, "config") or self.options.group then
      -- a component that hosts entries, such as a group or an include,
      -- leaves the reports to its entries
      local cb = self.innerFiber.runtime.callback
      if not (isSubclassOf(cb, EntryGroup) or isSubclassOf(cb, treeModule.EntryTree)) then
        self.loader:showLog(self, "reload")
      end
      return self.innerFiber:update(self.options.config, true)
    end
  end)
end

-- `async refresh()`
function Entry:refresh()
  return utils.try(self.ctx.host, function()
    if self.outerFiber or self.innerFiber then
      return
    end
    if self:_resolveInject() == nil then
      return
    end
    if self:_blocked() then
      return
    end
    return self:init()
  end)
end

-- `async update(options, create = false, force = false)`; the options are
-- assigned before this returns
function Entry:update(options, create, force)
  return utils.try(self.ctx.host, function()
    local legacy = copy(self.options)

    -- step 1: update options
    if create then
      self.options = options
    else
      Entry.merge(self.options, options)
    end

    -- step 2: resolve inject
    local changed = self:_resolveInject()
    if changed == nil then
      self:dispose()
      return
    end

    -- step 3: execute
    if self:_blocked() then
      self:dispose()
      return
    end

    -- the component and the outer fiber are bound when they are created
    local rebuild = changed or self.options.name ~= legacy.name
    local active = self.outerFiber or (self.innerFiber and self.innerFiber.uid)
    if rebuild or not active then
      -- the disposal is part of the init task, so a concurrent update joins it
      return self:init(active and self:dispose() or nil)
    end

    -- step 4: check if options are changed
    local keys, diff = {}, {}
    for _, key in ipairs(utils.ownKeys(self.options)) do
      keys[key] = true
    end
    for _, key in ipairs(utils.ownKeys(legacy)) do
      keys[key] = true
    end
    for key in pairs(keys) do
      if not utils.deepEqual(self.options[key], legacy[key]) then
        diff[#diff + 1] = key
      end
    end
    if #diff == 0 and not force then
      return
    end
    return self:_patchContext(diff)
  end)
end

function Entry:dispose()
  local fiber = self.outerFiber or self.innerFiber
  self.outerFiber = nil
  return self:_dispose(fiber)
end

function Entry:_dispose(fiber)
  self._disposing = true
  local ok, result = pcall(function()
    return fiber and fiber:dispose()
  end)
  self._disposing = false
  if not ok then
    error(result, 0)
  end
  return result
end

-- Register the outermost fiber of the entry on the fiber of the group that
-- holds the entry, which then disposes the entry with its own unload. Called
-- when the fiber is created, and when the entry is transferred to another group.
function Entry:_hold(fiber)
  local owner = self.ctx.fiber
  local previous = self._holder
  if previous and previous.owner == owner then
    return
  end
  -- An unloading group cannot take the entry over, and removes its members
  -- when it stops. Until then, the former group keeps the registration.
  if owner.uid == nil or owner.state == cordis.FiberState.UNLOADING then
    return
  end
  local holder = { fiber = fiber, owner = owner }
  holder.release = owner:effect(function()
    return cordis.future.internal(function()
      -- a former group may have taken the disposer before the entry was transferred out of it
      if self._holder == holder then
        return fiber:dispose()
      end
    end)
  end, "entry")
  if previous then
    previous.owner:leak(previous.release)
  else
    owner:leak(fiber.dispose)
  end
  self._holder = holder
end

-- Called when a fiber of the entry starts to be disposed, for whatever reason.
function Entry:_release(fiber)
  local holder = self._holder
  if not holder or holder.fiber.ctx.fiber ~= fiber then
    return
  end
  -- e.g. a component that disposes itself: its group no longer disposes it
  holder.owner:leak(holder.release)
  self._holder = nil
end

function Entry:getLocation(path)
  return self.parent.tree:getLocation(self.options.id, path or {})
end

function Entry:locateExpr(path, expr, offset)
  return self.parent.tree:locateExpr(self.options.id, path, expr, offset)
end

function Entry:traceValidation(err)
  local frames = self.getOuterStack()
  -- an entry outside any source has nothing to point to
  if #frames == 0 then
    return
  end
  local issues = {}
  for i, issue in ipairs(err.issues) do
    issues[i] = { message = issue.message, path = cordis.ValidationError.normalizePath(issue.path) }
  end
  local function locate(path)
    local full = { "config" }
    table.move(path, 1, #path, 2, full)
    return self:getLocation(full)
  end
  utils.relocate(err, utils.formatIssues("invalid config", issues, locate), frames)
end

function Entry:_reportImport(err)
  if treeModule.ModuleNotFoundError.is(err) then
    local location = self:getLocation({ "name" })
    if location then
      -- before the searcher hints
      local first, rest = err.message:match("^([^\n]*)(.*)$")
      utils.relocate(err, first .. " (at name, " .. utils.formatLocation(location) .. ")" .. rest, self.getOuterStack())
    end
  end
  self.ctx.logger:error(err)
end

-- per loader (keyed by its store, the same table whichever proxy reads
-- it), the entries whose init settled and whose fibers are still settling
local settling = setmetatable({}, { __mode = "k" })

-- `async init(disposeTask)`
function Entry:init(disposeTask)
  local host = self.ctx.host
  local task = self._initTask
  if not task then
    task = self:_init(disposeTask)
    self._initTask = task
  end
  local function release()
    -- `_init()` releases the task before creating fibers, whose apply may start another
    if self._initTask == task then
      self._initTask = nil
    end
  end
  return task:next(function()
    release()
    -- failures are already reported by the fiber; we only need it to settle
    -- upstream walks every entry (`loader.getTasks()`) after each one
    -- settles; here only the last of the entries settling at once walks them
    local loader = self.loader
    local key = loader.store
    settling[key] = (settling[key] or 0) + 1
    utils.spawn(host, function()
      pcall(function()
        cordis.tick(host)
        if self.outerFiber then
          self.outerFiber:await():await()
        end
        if self.innerFiber then
          self.innerFiber:await():await()
        end
      end)
      local left = settling[key] - 1
      settling[key] = left > 0 and left or nil
      if left > 0 or #loader:getTasks() > 0 then
        return
      end
      self.ctx.reflect:notify({ "loader" })
    end)
  end, function(err)
    release()
    error(err, 0)
  end)
end

-- `async _import()`: `{ component }`, or nil after reporting the failure
function Entry:_import()
  local host = self.ctx.host
  return utils
    .try(host, function()
      return self.parent.tree:import(self.options.name, self.getOuterStack)
    end)
    :next(function(exports)
      return { component = self.loader:unwrapExports(exports) }
    end, function(err)
      self:_reportImport(err)
    end)
end

-- `async _init(disposeTask)`: everything runs after the first `await`
function Entry:_init(disposeTask)
  local host = self.ctx.host
  return utils.spawn(host, function()
    -- `await disposeTask` yields even when there is nothing to wait for
    cordis.tick(host)
    cordis.await(disposeTask)
    -- a later update has already rebuilt the entry
    if self.outerFiber or (self.innerFiber and self.innerFiber.uid) then
      return
    end
    self._verdict = nil
    local name = self.options.name
    local exports = self:_import():await()
    -- the name may change while the module is imported
    if name ~= self.options.name then
      return self:_init():await()
    end
    if not exports or not self:_registered() or self:_blocked() then
      return
    end
    self:_patchContext({}):await()
    if name ~= self.options.name then
      return self:_init():await()
    end
    if not self:_registered() or self:_blocked() then
      return
    end
    self._initTask = nil
    if self:_hasOuter() then
      local component = { name = outer.name, apply = outer.apply, inject = self.inject }
      local entry = self
      self.outerFiber = self.ctx.registry:use(component, nil, {
        getOuterStack = self.getOuterStack,
        dispose = function(fiber)
          entry:_release(fiber)
        end,
      })
      self:_hold(self.outerFiber)
    else
      self:_start(self.ctx, exports.component)
    end
  end)
end

function Entry:_registered()
  return self.parent.tree.store[self.options.id] == self
end

function Entry:_start(ctx, component)
  local Context = cordis.Context
  if not self:_registered() then
    return
  end
  -- a group still starts; its verdict only reaches its children
  self:_resolveDisabled(ctx)
  if self.disabled or not self:_applyIntercept(ctx) then
    return
  end
  self.loader:showLog(self, "apply")
  local innerContext = ctx:extend({ [Context.intercept] = self._intercept })
  local entry = self
  self.innerFiber = innerContext.registry:use(component, self.options.config, {
    getOuterStack = self.getOuterStack,
    config = function(fiber, config, resolve)
      local ok, result = pcall(function()
        -- a group's config is the entry list itself, which the loader mutates
        -- in place and identifies by reference
        local callback = fiber.runtime.callback
        if type(callback) == "table" and callback[EntryGroup.key] then
          return resolve(config)
        end
        return resolve(utils.interpolate(fiber.ctx, config, entry.trace, { "config" }))
      end)
      if ok then
        return result
      end
      -- a config passed in at runtime is not written anywhere yet
      if cordis.ValidationError.is(result) and config == entry.options.config then
        entry:traceValidation(result)
      end
      error(result, 0)
    end,
    commit = function(fiber, config)
      local Config = fiber.runtime and fiber.runtime.Config
      local legacy = copy(entry.options)
      -- the receiver is kept
      if type(Config) == "table" and Config.simplify ~= nil then
        entry.options.config = Config:simplify(config)
      else
        entry.options.config = config
      end
      entry.parent.tree:commit({
        id = entry.options.id,
        group = entry.parent,
        options = entry.options,
        legacy = legacy,
      })
    end,
    dispose = function(fiber)
      entry:_release(fiber)
      entry:_onDispose(fiber)
    end,
  })
  if rawequal(ctx, self.ctx) then
    self:_hold(self.innerFiber)
  end
end

-- Called when the inner fiber starts to be disposed.
function Entry:_onDispose(fiber)
  -- case 1: the tree of the entry is being disposed
  local treeUid = self.parent.tree.ctx.fiber.uid
  if not treeUid or treeUid == 0 then
    return
  end

  self.loader:showLog(self, "unload")

  -- case 2: the parent of the entry is being unloaded, which takes the entry
  -- with it (the parent may still be registered, e.g. when it has lost a dependency)
  if fiber.parent.fiber.state == cordis.FiberState.UNLOADING then
    return
  end

  -- case 3: the entry is being removed (`EntryGroup:remove` unregisters it
  -- before disposing the fiber)
  if not self:_registered() then
    return
  end

  -- case 4: the fiber is disposed through `entry:dispose()`, e.g. on a config
  -- file update, an ancestor group being disabled, or a rebuild
  if self._disposing then
    return
  end

  -- otherwise, the component disposes itself: disable the entry
  local legacy = copy(self.options)
  self.options.disabled = true
  self._verdict = true
  self.parent.tree:commit({ id = self.options.id, group = self.parent, options = self.options, legacy = legacy })
  if self.outerFiber then
    self:dispose()
  end
end

return M
