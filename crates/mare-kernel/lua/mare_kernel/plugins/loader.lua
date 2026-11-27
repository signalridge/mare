-- The `loader` host plugin: the plugin tree is cordis-lua's
-- loader. boot.lua uses it with `{ component, prims }` as config:
--   component(name) the cordis component for a definition (registers its lua/ dir first)
--   prims.refused()   the definitions refused by the static provide check, for dump()
--   prims.profile_record(label, ms) one startup-profile entry; each apply is recorded
-- boot.lua passes `prims.rows()` (plugins/rows.rs) to apply_rows. Types and doc (typed,
-- documented primitives): types/mare/host/loader.lua, docs/en/extend/host-loader.md.
-- It provides the service `loader` (a cordis Loader) with three methods for the kernel:
--   loader:apply_rows(rows) -> Future the whole tree follows the rows (create, remove, disable,
--                                        reconfigure); start and `/reload config`
--   loader:replace(name, component) -> Future `/reload plugin`: registry:replace; if the new
--                                        definition fails to apply, the old one is put back
--   loader:dump() -> rows               state per row for `/plugins`, `mare doctor`, --dump
local cordis = require("cordis")
local L = require("cordis.loader")
local calls = require("mare_kernel.calls")

local Entry = L.Entry
local Future = cordis.Future
local FiberState = cordis.FiberState

local stateNames = {}
for name, value in pairs(FiberState) do
	stateNames[value] = name
end

local MareLoader = cordis.class("MareLoader", L.Loader)

-- Fallback when a direct caller's `component(name)` returns only one value (`boot.lua` already
-- passes `origin` alongside the component, so startup never hits this scan).
local function rowOrigin(self, name)
	for _, row in ipairs(self.root.data or {}) do
		if row.name == name then
			return row.origin
		end
	end
end

-- Fallback timing wrapper when a caller (such as pure Lua specs) supplies raw components
-- instead of `component.lua` wrappers.
local function timed(self, c, name, origin, record)
	if type(c) ~= "table" or cordis.class.is(c) or type(rawget(c, "apply")) ~= "function" then
		return
	end
	local apply = c.apply
	c.apply = function(...)
		local t = os.clock()
		local function done(...)
			local resolved = origin or rowOrigin(self, name) or "builtin"
			record(string.format("plugin %s (%s)", name, resolved), (os.clock() - t) * 1000)
			return ...
		end
		return done(apply(...))
	end
end

function MareLoader:constructor(ctx, config)
	L.Loader.constructor(self, ctx)
	-- Install the per-root parent->children fiber index before any row starts so loadingChildren,
	-- settle, and dump walk only each row's subtree instead of scanning the whole registry.
	calls.child_index(ctx)
	local component = assert(config and config.component, "loader: config.component is required")
	-- the definitions refused before the tree (static provide check), for dump
	self._refused = config.prims and config.prims.refused
	-- the current rows, for replace() (a reload by name uses the committed config)
	self._rowsNow = config.prims and config.prims.rows
	-- the only way the tree gets a component (cordis-lua LD-3); an unknown name raises
	-- "module '<name>' not found:", which the loader reports for that row only
	local record = config.prims and config.prims.profile_record
	local loader = self
	self.internal = {
		import = function(_, name)
			local c, origin, measured = component(name)
			if record and not measured then
				timed(loader, c, name, origin, record)
			end
			return c
		end,
	}
end

-- an `async` method in the library's sense: runs on the host, returns a Future
local function async(self, fn)
	return cordis.future.AsyncFunction.spawn(cordis.async(fn), self.ctx.host)
end

-- every fiber the tree runs now: { entry, fiber } for the outer and inner fiber of each entry
local function fibers(self)
	local out = {}
	for _, entry in ipairs(self:entries()) do
		-- not ipairs: outerFiber is nil for an entry without inject
		for _, key in ipairs({ "outerFiber", "innerFiber" }) do
			local f = entry[key]
			if f then
				out[#out + 1] = { entry = entry, fiber = f }
			end
		end
	end
	return out
end

-- whether the tree let go of `fiber` (its entry was removed or disabled, or now runs another)
local function released(self, entry, fiber)
	return self.store[entry.options.id] ~= entry
		or entry.options.disabled
		or (entry.innerFiber ~= fiber and entry.outerFiber ~= fiber)
end

-- until `fiber` is disposed: its disposal may start in a task of its own, so `inertia` can still be
-- nil right after the tree let go of it; `inertia` never rejects
local function disposed(host, fiber)
	while fiber.state ~= FiberState.DISPOSED do
		if fiber.inertia then
			cordis.await(fiber.inertia)
		else
			cordis.tick(host)
		end
	end
end

-- Collect any in-flight inertia futures on `entry`'s outer/inner fibers and their descendants.
local function collectEntryPending(rootCtx, idx, entry, pending)
	local top = entry.outerFiber or entry.innerFiber
	if not top then
		return
	end
	if top.inertia then
		pending[#pending + 1] = top.inertia
	end
	if idx[top] then
		calls.each_descendant(rootCtx, top, function(fiber)
			if fiber.inertia and fiber.ctx[Entry.key] == entry then
				pending[#pending + 1] = fiber.inertia
			end
		end)
	end
end

-- Wait for every entry task and any in-flight descendant fiber belonging to an entry of this
-- loader (e.g. `ctx:plugin(S)` whose `inject` became ready after another row provided it),
-- looping until quiet because a settling child can wake another entry or child.
local function settle(self)
	local host = self.ctx.host
	local rootCtx = self.ctx[cordis.Context.root]
	local idx = calls.child_index(rootCtx)
	while true do
		self:await():await()
		local pending = {}
		for _, entry in ipairs(self:entries()) do
			collectEntryPending(rootCtx, idx, entry, pending)
		end
		if #pending == 0 then
			return
		end
		Future.allSettled(host, pending):await()
	end
end

-- Partition `rows` by ascending `row.wave` when rows carry static topological wave numbers.
-- Preserves relative row order within each wave; rows without a numeric `wave` default to 0.
local function groupByWaves(rows)
	local buckets = {}
	local order = {}
	for _, row in ipairs(rows) do
		local w = type(row) == "table" and type(row.wave) == "number" and row.wave or 0
		local list = buckets[w]
		if not list then
			list = {}
			buckets[w] = list
			order[#order + 1] = w
		end
		list[#list + 1] = row
	end
	if #order <= 1 then
		return { rows }
	end
	table.sort(order)
	local waves = {}
	for i = 1, #order do
		waves[i] = buckets[order[i]]
	end
	return waves
end

-- Descendant fibers belonging to `entry` (other than its outerFiber and innerFiber), or nil if none.
local function entryDescendants(self, entry, rootCtx, idx)
	local top = entry.outerFiber or entry.innerFiber
	if not top then
		return nil
	end
	rootCtx = rootCtx or self.ctx[cordis.Context.root]
	idx = idx or calls.child_index(rootCtx)
	if not idx[top] then
		return nil
	end
	local out
	calls.each_descendant(rootCtx, top, function(f)
		if f ~= entry.outerFiber and f ~= entry.innerFiber and f.ctx[Entry.key] == entry then
			if not out then
				out = { f }
			else
				out[#out + 1] = f
			end
		end
	end)
	return out
end

-- First FAILED child component (`ctx:plugin` / named component) in `list`; anonymous
-- `ctx:inject` followers are excluded so an auxiliary hook failure does not fail the row.
local function firstFailedChild(list)
	if not list then
		return nil
	end
	for _, desc in ipairs(list) do
		if calls.is_child_component(desc) and desc.state == FiberState.FAILED then
			return desc
		end
	end
end

-- If `rows` keeps every currently held row table (`rawequal` and same scalar/config fields) and
-- only adds new unique-id rows, return the added rows so `apply_rows` creates them in waves
-- without re-reconciling the settled prefix; otherwise return nil for a full `update`.
-- Invariant: the exact same row table object only appears between startup phase 1 (`builtins`) and
-- phase 2 (`list`) in `boot.lua:start()`, because `prims.rows()` constructs fresh tables per call;
-- `meta` catches in-place `disabled`/`config`/`config_error` mutations (`internal/commit`,
-- self-dispose, `replace`), while in-place mutation of `name`/`inject` on reused tables is outside
-- this contract.
local function additiveAdded(self, rows)
	local oldData = self.root.data
	if #oldData == 0 or not self._rowMeta then
		return nil
	end
	local newById = {}
	for _, row in ipairs(rows) do
		if type(row) ~= "table" or type(row.id) ~= "string" or newById[row.id] ~= nil then
			return nil
		end
		newById[row.id] = row
	end
	local oldIds = {}
	for _, old in ipairs(oldData) do
		if type(old) ~= "table" or type(old.id) ~= "string" then
			return nil
		end
		local nxt = newById[old.id]
		local meta = self._rowMeta[old.id]
		if
			not nxt
			or not meta
			or not rawequal(nxt, old)
			or meta.disabled ~= old.disabled
			or meta.config ~= old.config
			or meta.config_error ~= old.config_error
		then
			return nil
		end
		oldIds[old.id] = true
	end
	local added = {}
	for _, row in ipairs(rows) do
		if not oldIds[row.id] then
			added[#added + 1] = row
		end
	end
	return added
end

-- The whole tree follows `rows`. The tree's own `await` only sees entries
-- still in the store, so the cleanup of a removed entry may still run when it returns; a FAILED
-- fiber is not retried by an update whose config did not change (upstream: a failed fiber
-- recovers only through update(), not restart()). apply_rows waits for the cleanups and updates
-- failed fibers with their config.
function MareLoader:apply_rows(rows)
	return async(self, function()
		local meta = {}
		for _, row in ipairs(rows) do
			if type(row) == "table" and type(row.id) == "string" then
				meta[row.id] = {
					disabled = row.disabled,
					config = row.config,
					config_error = row.config_error,
				}
			end
		end
		local before = fibers(self)
		-- failures from an earlier start or reload; one that fails now is not retried at once
		local failedBefore = {}
		for _, item in ipairs(before) do
			if item.fiber.state == FiberState.FAILED then
				failedBefore[item.fiber] = true
			end
		end
		local function createWave(waveRows)
			local tasks = {}
			for _, row in ipairs(waveRows) do
				tasks[#tasks + 1] = self.root:create(row):catch(function(err)
					self.ctx.logger:error(err)
				end)
			end
			Future.all(self.ctx.host, tasks):await()
		end
		-- Best-effort wave ordering on fresh and additive starts: create rows in ascending `wave`
		-- order with one scheduler turn between waves so synchronous wave-w providers usually
		-- publish before wave-(w+1) constructs fibers; never waits on async inertia, and any
		-- unsatisfied dependency (async, late, or cross-wave) falls back to Cordis PENDING.
		local added = additiveAdded(self, rows)
		if added ~= nil then
			self.root.data = rows
			if #added > 0 then
				local waves = groupByWaves(added)
				for i, waveRows in ipairs(waves) do
					createWave(waveRows)
					if i < #waves then
						cordis.tick(self.ctx.host)
					end
				end
			end
		elseif #self.root.data == 0 then
			local waves = groupByWaves(rows)
			if #waves > 1 then
				local first = self.root:update(waves[1])
				self.root.data = rows
				first:await()
				cordis.tick(self.ctx.host)
				for i = 2, #waves do
					createWave(waves[i])
					if i < #waves then
						cordis.tick(self.ctx.host)
					end
				end
			else
				self.root:update(rows):await()
			end
		else
			self.root:update(rows):await()
		end
		self._rowMeta = meta
		settle(self)
		if added == nil then
			for _, item in ipairs(before) do
				if released(self, item.entry, item.fiber) then
					disposed(self.ctx.host, item.fiber)
				end
			end
		end
		local retried = false
		for _, entry in ipairs(self:entries()) do
			local f = entry.innerFiber
			if f and failedBefore[f] and f.uid and f.state == FiberState.FAILED and not entry.options.disabled then
				retried = true
				pcall(function()
					cordis.await(f:update(entry.options.config, true))
				end)
			end
		end
		if retried then
			settle(self)
		end
	end)
end

-- Remove the rows `ids` one at a time, in the order given, each one's fiber disposed (its cleanups
-- done) before the next goes: a reload's teardown, in the old path's order, without a whole-tree
-- update and settle per row. The rows leave the tree's row data; a later apply_rows that lists
-- them creates them again.
function MareLoader:remove_rows(ids)
	return async(self, function()
		for _, id in ipairs(ids) do
			local entry = self.store[id]
			if entry then
				local fiber = entry.outerFiber or entry.innerFiber
				self.root:remove(id)
				if fiber then
					disposed(self.ctx.host, fiber)
				end
			end
		end
	end)
end

-- the inner fiber and entry for row `name`, if any
local function fiberOf(self, name)
	local entry = self.store[name]
	return entry and entry.innerFiber, entry
end

---The fiber row `name` runs in (the stall report reads its state and age), nil off the tree.
function MareLoader:fiber(name)
	return (fiberOf(self, name))
end

function MareLoader:replace(name, component)
	return async(self, function()
		local fiber, entry = fiberOf(self, name)
		if not fiber or not fiber.runtime then
			error(cordis.Error.new("loader: plugin " .. tostring(name) .. " is not running"), 0)
		end
		local old = fiber.runtime.callback
		-- the restart runs with the plugin's current config (a runtime option write that
		-- reloads the plugin, a config change), not the one the fiber started with; one restart
		if self._rowsNow then
			for _, row in ipairs(self._rowsNow()) do
				if row.id == name and row.config ~= nil then
					local entry = self.store[name]
					if entry then
						entry.options.config = row.config
					end
					fiber.config = row.config
					break
				end
			end
		end
		local registry = self.ctx.registry
		local fibers = registry:replace(old, component)
		local failure
		for _, f in ipairs(fibers) do
			local ok, err = pcall(function()
				cordis.await(f:await())
			end)
			if not ok then
				failure = failure or err
			end
		end
		settle(self)
		if failure == nil and entry then
			local failed = firstFailedChild(entryDescendants(self, entry))
			if failed then
				failure = failed.error or cordis.Error.new("child component failed")
			end
		end
		if failure ~= nil then
			-- the new definition did not apply: the old one runs again
			for _, f in ipairs(registry:replace(component, old)) do
				pcall(function()
					cordis.await(f:await())
				end)
			end
			settle(self)
			error(failure, 0)
		end
	end)
end

local function buildRowItem(self, row, descs, includeEffects)
	local item = {
		id = row.id,
		origin = row.origin,
		path = row.path,
		overrides = row.overrides,
		refused = row.refused,
		disabled = row.disabled == true,
	}
	local fiber, entry = fiberOf(self, row.id)
	if row.config_error then
		item.state = "FAILED"
		item.error = row.config_error
	elseif fiber then
		item.state = stateNames[fiber.state]
		if fiber.state == FiberState.PENDING then
			item.missing = fiber:pending()
		end
		local provides = {}
		local rowDescs
		if descs == true then
			rowDescs = entry and entryDescendants(self, entry) or nil
		elseif descs and entry then
			rowDescs = descs[entry]
		end
		local seen = (rowDescs or fiber.provisions:size() > 1) and {} or nil
		local function addProvisions(f)
			if f.provisions:size() == 0 then
				return
			end
			for _, impl in ipairs(f.provisions:values()) do
				local name = impl.name
				if not seen then
					provides[#provides + 1] = name
				elseif not seen[name] then
					seen[name] = true
					provides[#provides + 1] = name
				end
			end
		end
		addProvisions(fiber)
		if rowDescs then
			for _, desc in ipairs(rowDescs) do
				addProvisions(desc)
			end
			if item.state ~= "FAILED" then
				local failed = firstFailedChild(rowDescs)
				if failed then
					item.state = "FAILED"
					item.error = calls.error_message(failed.error)
				end
			end
		end
		item.provides = provides
		if includeEffects then
			item.effects = #fiber:getEffects()
		end
	elseif not item.disabled then
		-- not started: waiting for its definition (import) or failed to import
		item.state = "NONE"
	end
	return item
end

function MareLoader:dump(opts)
	local includeEffects = not (opts and opts.effects == false)
	-- Collect descendant fibers per entry via the subtree index (O(entries + descendants)).
	local rootCtx = self.ctx[cordis.Context.root]
	local idx = calls.child_index(rootCtx)
	local descendants
	for _, entry in ipairs(self:entries()) do
		local list = entryDescendants(self, entry, rootCtx, idx)
		if list then
			if not descendants then
				descendants = {}
			end
			descendants[entry] = list
		end
	end
	local out = {}
	for _, row in ipairs(self.root.data) do
		out[#out + 1] = buildRowItem(self, row, descendants, includeEffects)
	end
	-- refused definitions are not rows: listed after them, never run
	for _, r in ipairs(self._refused and self._refused() or {}) do
		out[#out + 1] = { id = r.id, origin = r.origin, path = r.path, refused = r.refused, disabled = true }
	end
	return out
end

function MareLoader:row_state(id)
	local row
	for _, r in ipairs(self.root.data) do
		if r.id == id then
			row = r
			break
		end
	end
	if not row then
		for _, r in ipairs(self._refused and self._refused() or {}) do
			if r.id == id then
				return { id = r.id, origin = r.origin, path = r.path, refused = r.refused, disabled = true }
			end
		end
		return nil
	end
	return buildRowItem(self, row, true, false)
end

return {
	name = "loader",
	apply = function(ctx, config)
		ctx:use(MareLoader, config)
	end,
	-- exposed for the host plugin's own spec
	MareLoader = MareLoader,
}
