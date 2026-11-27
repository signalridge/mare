-- Ported from cordis packages/loader/src/index.ts (MIT, (c) Shigma)
--
-- `require("cordis.loader")` returns the module table: `Loader` (also
-- `default`), `Entry`, `EntryGroup`, `Group`, `EntryTree`, `LocalRealm`,
-- `ModuleNotFoundError`, `EntryValidationError` and the helpers of
-- `config/utils.ts` and `config/isolate.ts`.
--
-- Deviations: see docs/deviations.md, loader section. Not ported: Node's
-- internal loader (`internal.ts`: `loader.internal` stays nil unless the host
-- installs an importer), `resolve.ts`, and `CORDIS_SHARED` in `envData`.
local cordis = require("cordis")
local utils = require("cordis.loader.utils")
local isolate = require("cordis.loader.isolate")
local treeModule = require("cordis.loader.tree")
local groupModule = require("cordis.loader.group")
local entryModule = require("cordis.loader.entry")

local Entry = entryModule.Entry
local EntryGroup = groupModule.EntryGroup
local EntryTree = treeModule.EntryTree

treeModule._bind(Entry, EntryGroup)
groupModule._bind(Entry, EntryTree)
entryModule._bind(EntryGroup)

local M = {}

---Get the component exported by a module (internal.ts:31-39).
function M.unwrapExports(exports)
  if exports == nil then
    return exports
  end
  if type(exports) == "table" and not cordis.class.is(exports) and exports.default ~= nil then
    return exports.default
  end
  return exports
end

local Loader = cordis.class("Loader", EntryTree)
M.Loader = Loader
M.default = Loader

function Loader:constructor(ctx)
  local Service = cordis.Service
  EntryTree.constructor(self, ctx)
  self.envData = { startTime = (os and os.time and os.time() * 1000) or (ctx.host.now and ctx.host.now()) or 0 }
  self.name = "loader"
  -- Node's internal ESM loader in upstream; a host may install `{ import = fn }`
  self.internal = nil

  self[Service.tracker] = {
    associate = "loader",
    property = "ctx",
    noShadow = true,
  }

  ctx.reflect:provide("loader", self, self[Service.check])
end

function Loader:commit(_change)
  -- the root tree lives in memory only; there is nothing to persist to
end

Loader[cordis.symbols.check] = function(self)
  local Service = cordis.Service
  local config = Service[Service.resolveIntercept](self)
  if config.await and #self:getTasks() > 0 then
    return false
  end
  return true
end

function Loader:showLog(entry, type)
  local Context = cordis.Context
  if entry.options.group or not entry.parent.tree.enableLogs then
    return
  end
  local logger = self.ctx[Context.root].logger
  if logger then
    logger("loader"):info("%s component %C", type, entry.options.name)
  end
end

function Loader:locate(fiber)
  fiber = fiber or self.ctx.fiber
  local entry = fiber.ctx[Entry.key]
  return entry and entry.id or nil
end

function Loader:unwrapExports(exports)
  return M.unwrapExports(exports)
end

M.Entry = Entry
M.EntryGroup = EntryGroup
M.Group = groupModule.Group
M.EntryTree = EntryTree
M.ModuleNotFoundError = treeModule.ModuleNotFoundError
M.LocalRealm = isolate.LocalRealm
M.accessGlobal = isolate.accessGlobal
M.patchIsolate = isolate.patchIsolate
for _, name in ipairs({
  "evaluate",
  "interpolate",
  "isLuaExpr",
  "swap",
  "formatLocation",
  "formatIssue",
  "formatIssues",
  "relocate",
  "validateOptions",
  "EntryValidationError",
}) do
  M[name] = utils[name]
end

return M
