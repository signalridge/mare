-- The kernel's startup fragment: the cordis host on mare.task and the root
-- context. `prims` are kernel-internal primitives passed in from Rust (lua_env/cordis_host.rs,
-- Kernel::cordis_boot); they are never a global or a module, so plugins cannot get at the
-- host and cannot build a second tree.
--
--   prims.capture() -> scope   the running code's execution context (owner + tool context)
--   scope:is_current()         whether that is the context running now
--   scope:run(fn,...) call fn in that context and wait for it (async)
--   scope:spawn(fn) start fn as a task in that context
--   prims.spawn_root(fn) start fn as a task in the host's own context, owned by no plugin
return function(prims)
	local cordis = require("cordis")

	local host = {}

	-- the library's own tasks (job-queue drainers, deferred starts): host context, no owner
	function host.spawn(fn)
		prims.spawn_root(fn)
	end

	-- a latch is a cancellation token: cancel wakes the waiters without resuming them inline
	local latch_mt = {
		__index = {
			open = function(self)
				self._token:cancel()
			end,
			wait = function(self)
				self._token:wait()
			end,
		},
	}
	function host.latch()
		return setmetatable({ _token = mare.task.token() }, latch_mt)
	end

	-- mare.task.sleep takes whole milliseconds (it truncates): round a positive fraction up so
	-- that a timer never fires before its deadline
	-- with a latch, the sleep also ends when the latch opens:
	-- a cleared timer's task ends at once
	function host.sleep(ms, latch)
		local token = latch and latch._token
		ms = (ms and ms > 0) and math.ceil(ms) or 0
		if token then
			mare.task.sleep(ms, token)
		else
			mare.task.sleep(ms)
		end
	end

	-- monotonic milliseconds (mare.task.now is wall-clock epoch time)
	function host.now()
		return mare.task.clock()
	end

	-- User callbacks run in the context they were registered in. prims.capture()
	-- returns one shared scope for the host's own context, so its wrapper is built once too
	-- (startup opt); every other capture gets its own
	local rootScope, rootWrapper
	local function wrap(scope)
		return {
			run = function(fn, ...)
				if scope:is_current() then
					return fn(...)
				end
				-- Keep raised Lua values intact across the Rust scope boundary.
				local results = scope:run(function(...)
					return table.pack(pcall(fn, ...))
				end, ...)
				if not results[1] then
					error(results[2], 0)
				end
				return table.unpack(results, 2, results.n)
			end,
			-- A handle that can cancel the task (a fiber disposed while applying)
			spawn = function(fn)
				local task = scope:spawn(fn)
				return {
					cancel = function()
						task:cancel()
					end,
				}
			end,
		}
	end
	function host.capture()
		local scope = prims.capture()
		if rawequal(scope, rootScope) then
			return rootWrapper
		end
		local wrapper = wrap(scope)
		if rootScope == nil and scope:is_root() then
			rootScope, rootWrapper = scope, wrapper
		end
		return wrapper
	end

	local root = cordis.Context.new({ host = host })
	local booted = { cordis = cordis, host = host, root = root }
	-- One bus: mare's event dispatch on the root's events service; the Rust emitters call it
	booted.bus = require("mare_kernel.bus")(root, prims)
	if not prims.rows then
		return booted -- the host alone
	end

	-- the library's logger (load failures, cleanup errors) goes to mare's log
	-- the error a fiber's apply failed with is reported as "plugin X failed", not again here
	local function applyError(err)
		for _, rt in ipairs(root.registry:values()) do
			for _, fiber in ipairs(rt.fibers:values()) do
				if fiber.error ~= nil and rawequal(fiber.error, err) then
					return true
				end
			end
		end
		return false
	end
	root.logger:exporter({
		levels = { default = cordis.LoggerLevel.WARN },
		export = function(message)
			local text = cordis.Logger.format({}, message)
			pcall(mare.log, message.type == "error" and "error" or "warn", "[" .. message.name .. "] " .. text)
			-- a plugin's error that cordis only logs (an effect cleanup that raised, a task or timer
			-- callback through the task host plugin) is shown like the kernel's own callbacks':
			-- problems, log and `kernel/error`
			if message.type == "error" and prims.handler_failed then
				local err = message.args and message.args[1]
				if not applyError(err) then
					pcall(prims.handler_failed, message.name, "callback", text)
				end
			end
		end,
	})

	-- host plugins, outside the plugin tree
	local calls = require("mare_kernel.calls")
	calls.coded = prims.coded_error
	local proto = require("cordis.proto")
	root:on("internal/provide", function(_, name, value)
		if type(name) ~= "string" then
			return
		end
		-- a provide through a traced ctx hands over a traced proxy of the instance mare.Service
		-- admitted (an inline `Class.new(ctx)` in an apply): the admitted mark is on its target
		local raw = value
		while proto.isProxy(raw) do
			raw = proto.proxyTarget(raw)
		end
		-- admitted for this name: an admitted instance raw-provided under another declared name is not
		if calls.admitted[raw] == name then
			return
		end
		-- Every declared service must pass mare.Service admission, regardless of which ctx
		-- (including root) calls provide/set.
		if prims.contracts.declared(name) then
			error(
				calls.coded(
					"invalid_arguments",
					("service `%s` is declared: provide it through mare.Service (mare_lib.service)"):format(name)
				),
				0
			)
		end
	end, { global = true })
	-- mare.Service: built once over prims and reachable to
	-- plugins as `mare.Service` (`cordis.class("X", mare.Service)`)
	local service_class, bind_service_root = require("mare_kernel.service")(prims)
	booted.Service = service_class
	booted.bind_service_root = bind_service_root
	if type(mare) == "table" then
		mare.Service = booted.Service
	end

	root:use(require("mare_kernel.plugins.task"), { prims = prims })
	-- contract admission and the guarded provision (an RPC
	-- connection's remote services are provided through it)
	root:use(require("mare_kernel.plugins.contracts"), { prims = prims })
	-- every RPC connection is a fiber; remote hooks are listeners on it
	root:use(require("mare_kernel.plugins.rpc"), { prims = prims })
	-- `ctx.bus`, a native plugin's entry for mare events (the Rust entry: taps,
	-- blocked turns, record)
	root:use(require("mare_kernel.plugins.bus"), { prims = prims, bus = booted.bus })
	-- Also enforce event declarations on direct `ctx:emit`/`bail`/`waterfall`/`parallel`/`serial`
	-- (skipping only the fixed set of cordis-internal lifecycle events and non-string symbols).
	if prims.events and prims.events.check_dispatch then
		local check_dispatch = prims.events.check_dispatch
		local symbols = require("cordis.symbols")
		local CORDIS_INTERNAL_EVENTS = {
			["internal/commit"] = true,
			["internal/config"] = true,
			["internal/dispatch"] = true,
			["internal/get"] = true,
			["internal/listener"] = true,
			["internal/plugin"] = true,
			["internal/provide"] = true,
			["internal/replace"] = true,
			["internal/service"] = true,
			["internal/set"] = true,
			["internal/status"] = true,
			["internal/update"] = true,
		}
		local isSymbol = symbols.isSymbol
		local nullSym = symbols.null
		local function is_this_arg(x)
			local t = type(x)
			return t == "function" or (t == "table" and not isSymbol(x) and x ~= nullSym)
		end
		local function resolve_ctx_owner(ctx)
			if type(ctx) ~= "table" or not cordis.Context.is(ctx) then
				return nil, false
			end
			local fiber = calls.caller_fiber(ctx)
			for f in calls.fibers_up(fiber) do
				local owner = calls.owners[f]
				if owner ~= nil then
					return owner, true
				end
			end
			return nil, true
		end
		local events_svc = root.events
		while proto.isProxy(events_svc) do
			events_svc = proto.proxyTarget(events_svc)
		end
		for _, mode in ipairs({ "emit", "bail", "waterfall", "parallel", "serial" }) do
			local orig = proto.get(events_svc, mode, events_svc)
			if type(orig) == "function" then
				proto.defineOwn(events_svc, mode, function(ev_self, ...)
					local a1, a2 = ...
					local name = type(a1) == "string" and a1 or (is_this_arg(a1) and a2 or a1)
					if type(name) == "string" and not CORDIS_INTERNAL_EVENTS[name] then
						local owner, known = resolve_ctx_owner(ev_self.ctx)
						check_dispatch(mode, name, owner, known)
					end
					return orig(ev_self, ...)
				end)
			end
		end
	end
	-- the `kernel` service (introspection and reload, the old Ctx methods); a mare.Service, so it
	-- comes after `mare.Service` and the contracts host plugin
	booted.kernelFiber = root:use(require("mare_kernel.plugins.kernel")(prims, booted.Service))
	-- security decision points as a registration service
	booted.policyFiber = root:use(require("mare_kernel.plugins.policy")(prims, booted.Service, booted.bus))
	-- unified hooks.subscribe service over bus events and policy points
	booted.hooksFiber = root:use(require("mare_kernel.plugins.hooks")(prims, booted.Service))
	-- every row's fiber, weakly, by name: one whose unload is in flight has left its runtime's
	-- list, and booted.unsettled() still names it
	local rowFibers = setmetatable({}, { __mode = "k" })
	-- every definition is a native cordis component
	local components_of = require("mare_kernel.component")(prims, root, rowFibers)
	-- the last apply error per plugin (the failure report, plugins/rows.rs)
	booted.apply_errors = components_of.errors
	-- the host's own fiber: the Rust host's native registrations
	booted.hostFiber = require("mare_kernel.host_fiber")(root, prims, booted.bus, booted.Service)

	-- a row's component: its lua/ dir first (at row time), then the definition
	-- the component each plugin was last built from: what a failed reload puts back
	local components = {}
	local function component(row)
		prims.module_dir(row.name)
		local def = prims.definition(row.name)
		if not def then
			error("module '" .. row.name .. "' not found:", 0)
		end
		local c = components_of.component(def, row.origin)
		components[row.name] = c
		return c
	end
	booted.component = component

	-- The plugin tree: the `loader` host plugin over the rows.
	local origins = {}
	local function rows()
		local list = prims.rows()
		for _, row in ipairs(list) do
			origins[row.id] = row.origin
		end
		return list
	end
	-- boot itself is synchronous; start / reload (async, from Rust) wait for the loader's fiber
	local loaderFiber = root:use(require("mare_kernel.plugins.loader"), {
		prims = prims,
		component = function(name)
			local origin = origins[name]
			return component({ name = name, origin = origin }), origin, prims.profile_record ~= nil
		end,
	})
	local function loader()
		cordis.await(loaderFiber)
		local service = root.reflect:get("loader", false)
		if not service then
			error(
				("the loader host plugin is not running (%s): %s"):format(
					tostring(loaderFiber.state),
					tostring(loaderFiber.error)
				),
				0
			)
		end
		return service
	end
	booted.loader = loader
	-- every row's state from one dump (a loop over the rows reads this, not a dump per row); a
	-- refused same-name definition is listed after the rows (diagnostics, no state) and must not
	-- erase the running row's state (an `override = true` replacement leaves one)
	-- Internal dumps (`start`, `reload`, `states`, `failed`, `pending`) never read `.effects`,
	-- so skip `fiber:getEffects()` table construction on every row.
	local DUMP_NO_EFFECTS = { effects = false }
	local function states(dump)
		local out, errs = {}, {}
		for _, item in ipairs(dump or loader():dump(DUMP_NO_EFFECTS)) do
			if not item.refused then
				out[item.id] = item.state
				errs[item.id] = item.error
			end
		end
		return out, errs
	end
	local function stateOf(id)
		return states()[id]
	end
	local function failed(dump)
		local out = {}
		for _, item in ipairs(dump or loader():dump(DUMP_NO_EFFECTS)) do
			if item.state == "FAILED" then
				local err = components_of.errors[item.id] or item.error
				out[#out + 1] = "plugin " .. item.id .. " failed" .. (err and (": " .. err) or "")
			end
		end
		return out
	end

	-- interim (until the kernel's Rust readers move to cordis): a service by name from the root,
	-- for Kernel::service when the tree runs on cordis
	-- Bounded shutdown (`Kernel::shutdown_until`): `booted.close()` closes the tree for good
	-- (`root:close()`) and returns its Future (the same one when called again);
	-- `booted.unsettled()` lists, by plugin name, what has not finished unloading, for Rust to
	-- report as unconfirmed when its deadline passes. A fiber leaves its runtime's list when its
	-- disposal starts, so `close()` takes a snapshot of every fiber first.
	local function nameOf(fiber)
		return calls.plugin_of(fiber, root.fiber)
	end
	local function snapshot()
		local out = {}
		calls.each_fiber(root, function(fiber)
			out[#out + 1] = fiber
		end)
		return out
	end
	-- what waits now: every enabled row left PENDING, with the services it waits for (a reload's
	-- report and a config reload's, Kernel::waiting, whichever path changed the tree)
	function booted.pending(dump)
		local out = {}
		for _, item in ipairs(dump or loader():dump(DUMP_NO_EFFECTS)) do
			if item.state == "PENDING" and not item.disabled then
				local waits = item.missing
				out[#out + 1] = "plugin "
					.. item.id
					.. " is waiting for "
					.. ((waits and #waits > 0) and table.concat(waits, ", ") or "a service")
			end
		end
		return out
	end

	local closing, closeSnapshot
	function booted.close()
		if not closing then
			closeSnapshot = snapshot()
			closing = root:close()
		end
		return closing
	end
	-- Lifecycle stalls (`Kernel::stalled`, rutis's "still waiting" report): when each fiber
	-- entered its current state, stamped when it is created (`internal/plugin`) and at every
	-- change (`internal/status`); weak, a fiber that is gone drops out. Observation only: a stuck
	-- fiber is reported, never aborted (an old generation may still hold what it cleans up).
	local entered = setmetatable({}, { __mode = "k" })
	-- the labels of a fiber's effects when its dispose began: its unload takes the effects off
	-- the fiber and runs their cleanups concurrently, so `getEffects()` no longer lists them
	local unloadEffects = setmetatable({}, { __mode = "k" })
	local function stamp(fiber)
		entered[fiber] = mare.task.clock()
	end
	root:on("internal/plugin", function(fiber)
		stamp(fiber)
		if fiber.uid == nil then
			local labels = {}
			for _, m in ipairs(fiber:getEffects()) do
				if not calls.kernel_effects[m] then
					labels[#labels + 1] = tostring(m.label)
				end
			end
			unloadEffects[fiber] = labels
		end
	end, { global = true })
	root:on("internal/status", stamp, { global = true })
	local FS = cordis.FiberState
	local STUCK = { [FS.LOADING] = "LOADING", [FS.UNLOADING] = "UNLOADING", [FS.PENDING] = "PENDING" }
	-- What has been LOADING, UNLOADING or PENDING for `threshold_ms` or longer, one entry per
	-- (plugin, state): `{ plugin, state, ms, effects, missing }`. `effects` are effect labels
	-- (`ctx:effect(fn, label)`): for UNLOADING the effects it held when its dispose began (whose
	-- cleanups are what it waits for), for LOADING what its apply set up so far. PENDING counts only a row's own fiber
	-- (a plugin waiting for a service it injects, `missing`): an optional `ctx:inject` child that
	-- waits is the plugin working as written. A fiber seen for the first time is stamped now.
	function booted.stalled(threshold)
		local now = mare.task.clock()
		local out, byKey, visited = {}, {}, {}
		local function add(list, seen, v)
			if not seen[v] then
				seen[v] = true
				list[#list + 1] = v
			end
		end
		local function visit(fiber, name, isRow)
			if visited[fiber] then
				return
			end
			visited[fiber] = true
			local state = STUCK[fiber.state]
			if not state or (state == "PENDING" and not isRow) then
				return
			end
			local since = entered[fiber]
			if not since then
				entered[fiber] = now
				return
			end
			local ms = now - since
			if ms < threshold then
				return
			end
			name = name or nameOf(fiber) or "?"
			local key = name .. "\0" .. state
			local e = byKey[key]
			if not e then
				e = { plugin = name, state = state, ms = ms, effects = {}, missing = {}, _e = {}, _m = {} }
				byKey[key] = e
				out[#out + 1] = e
			elseif ms > e.ms then
				e.ms = ms
			end
			local held = state == "UNLOADING" and unloadEffects[fiber]
			if held then
				for _, label in ipairs(held) do
					add(e.effects, e._e, label)
				end
			else
				for _, m in ipairs(fiber:getEffects()) do
					if not calls.kernel_effects[m] then
						add(e.effects, e._e, tostring(m.label))
					end
				end
			end
			if state == "PENDING" then
				for _, svc in ipairs(fiber:pending()) do
					add(e.missing, e._m, svc)
				end
			end
		end
		local loaderService = root.reflect:get("loader", false)
		if loaderService then
			for _, row in ipairs(loaderService.root.data) do
				local fiber = not row.disabled and loaderService:fiber(row.id)
				if fiber then
					visit(fiber, row.id, true)
				end
			end
		end
		calls.each_fiber(root, function(fiber)
			visit(fiber)
		end)
		-- an unload in flight has left its runtime's list (booted.unsettled)
		for fiber, name in pairs(rowFibers) do
			visit(fiber, name)
		end
		for _, e in ipairs(out) do
			e._e, e._m = nil, nil
		end
		table.sort(out, function(a, b)
			if a.plugin ~= b.plugin then
				return a.plugin < b.plugin
			end
			return a.state < b.state
		end)
		return out
	end

	function booted.unsettled()
		local seen, out = {}, {}
		for _, fiber in ipairs(closeSnapshot or snapshot()) do
			if fiber.state ~= cordis.FiberState.DISPOSED then
				local name = nameOf(fiber) or "?"
				if not seen[name] then
					seen[name] = true
					out[#out + 1] = name
				end
			end
		end
		-- a row whose unload began before the close (a reload, a plugin turned off) is in no
		-- runtime's list any more
		for fiber, name in pairs(rowFibers) do
			if fiber.state ~= cordis.FiberState.DISPOSED and not seen[name] then
				seen[name] = true
				out[#out + 1] = name
			end
		end
		table.sort(out)
		return out
	end

	-- Test harness (tests/common `expose`, global `C`): `C.cordis` is the native root ctx.
	-- The harness exposes this root ctx directly as `C`.
	local testCtx = { cordis = root }
	function booted.test_ctx()
		return testCtx
	end

	function booted.service(name)
		return root.reflect:get(name, false)
	end

	-- The built-in rows settle first, then the rest, as on the old path where user and project
	-- plugins applied after the built-ins.
	-- A row's lua/ dir is registered when the row is built (component()) and dropped by its owner's
	-- cleanup, which a row that never applied (PENDING) does not have. After the tree follows a row
	-- list: a row turned off (enabled = false) drops its dir; a row that left the tree for good drops
	-- its dir and its component (never a rollback target for a later plugin of the same name).
	-- unload_modules is idempotent after an owner's cleanup.
	local function releaseRows(list, previous)
		local present = {}
		for _, row in ipairs(list) do
			present[row.id] = true
			if row.disabled and prims.unload_modules then
				prims.unload_modules(row.id)
			end
		end
		for _, id in ipairs(previous) do
			if not present[id] then
				if prims.unload_modules then
					prims.unload_modules(id)
				end
				components[id] = nil
			end
		end
	end
	local function treeIds()
		local ids = {}
		for _, row in ipairs(loader().root.data) do
			ids[#ids + 1] = row.id
		end
		return ids
	end

	local hostServiceFibers = {}
	local function mount_host_services()
		cordis.await(booted.kernelFiber)
		for _, svc_name in ipairs(prims.contracts.host_only_services()) do
			if not hostServiceFibers[svc_name] and root.reflect:getImpl(svc_name, false) == nil then
				local HostSvc = cordis.class("HostService_" .. svc_name, booted.Service)
				function HostSvc:constructor(ctx)
					local owner = prims.owner.open(svc_name, "root")
					ctx:effect(function()
						return function()
							prims.owner.close(owner)
						end
					end, "owner")
					prims.owner.enter(owner, HostSvc.super.constructor, self, ctx, svc_name)
				end
				hostServiceFibers[svc_name] = root:use(HostSvc)
			end
		end
		for _, fiber in pairs(hostServiceFibers) do
			cordis.await(fiber)
		end
	end

	local function start()
		mount_host_services()
		local list = rows()
		local previous = treeIds()
		local builtins = {}
		for _, row in ipairs(list) do
			if row.origin == "builtin" then
				builtins[#builtins + 1] = row
			end
		end
		if #builtins > 0 and #builtins < #list then
			loader():apply_rows(builtins):await()
			if prims.after_builtins then
				prims.after_builtins()
			end
		end
		loader():apply_rows(list):await()
		releaseRows(list, previous)
		local dump = loader():dump(DUMP_NO_EFFECTS)
		local pending = {}
		for _, item in ipairs(dump) do
			if item.state == "PENDING" and item.missing and item.missing[1] then
				pending[#pending + 1] = "plugin "
					.. item.id
					.. " not started: required service `"
					.. item.missing[1]
					.. "` is not available"
			end
		end
		return { failures = failed(dump), pending = pending, dump = dump }
	end

	-- `/reload [name]`: Rust has prepared and committed the new definitions. One plugin:
	-- `loader:replace` (its dependents reload as cordis sees the provider change; on failure the
	-- old definition is put back). All: the tree follows the new row list.
	-- `/reload [name]`: Rust has prepared and committed the new definitions; `targets` are the
	-- plugins to restart (the old path's set: the named one and its dependents, every plugin on a
	-- full reload, the changed ones on a config reload).
	-- One running plugin: `loader:replace` (its dependents follow the provider change; if the new
	-- definition fails, the old one is put back). Otherwise: the rows without the targets first
	-- (the loader disposes them and waits; cordis unloads dependents before their providers), then
	-- the whole row list, which builds each target again from its new definition and config.
	local function reload(name, targets)
		local list = rows()
		local failures = {}
		local restored, kept, pending = {}, {}, nil
		local before = {}
		for id, n in pairs(components_of.applies) do
			before[id] = n
		end
		-- the rows in the tree before this reload (gone rows, fingerprint changes, restores)
		local old = {}
		for _, row in ipairs(loader().root.data) do
			old[row.id] = row
		end
		-- each row's state before this reload (what `applied` is judged against)
		local pre = states()
		local targeted = {}
		for _, t in ipairs(targets or {}) do
			targeted[t] = true
		end
		local running = false
		if name then
			for _, item in ipairs(loader():dump(DUMP_NO_EFFECTS)) do
				if item.id == name and item.state ~= nil and item.state ~= "NONE" then
					running = not item.disabled
				end
			end
		end
		local function emit_before_reload(seed)
			local changing = {}
			for id, v in pairs(seed) do
				if v then
					changing[id] = true
				end
			end
			if next(changing) == nil then
				return
			end
			local providers = {}
			for _, item in ipairs(loader():dump(DUMP_NO_EFFECTS)) do
				for _, svc in ipairs(item.provides or {}) do
					providers[svc] = item.id
				end
			end
			local grew = true
			while grew do
				grew = false
				for id, comp in pairs(components) do
					if not changing[id] and type(comp) == "table" and type(comp.inject) == "table" then
						for _, svc in ipairs(comp.inject) do
							local owner = providers[svc]
							if owner and changing[owner] then
								changing[id] = true
								grew = true
								break
							end
						end
					end
				end
			end
			local plugins = {}
			for id in pairs(changing) do
				plugins[#plugins + 1] = id
			end
			table.sort(plugins)
			local errs = booted.bus.emit("kernel/before-reload", { plugins = plugins })
			for _, item in ipairs(errs or {}) do
				failures[#failures + 1] = "kernel/before-reload listener "
					.. tostring(item[1])
					.. " failed: "
					.. tostring(item[2])
			end
		end
		local function rebuild_mod_deps(mod_deps)
			-- Rebuild module dependents after the provider has rebuilt, skipping any
			-- already restarted by Cordis service injection.
			for _, dep in ipairs(mod_deps) do
				if
					dep ~= name
					and stateOf(dep) == "ACTIVE"
					and (components_of.applies[dep] or 0) == (before[dep] or 0)
				then
					local dep_prev = components[dep]
					local dep_ok, dep_err = pcall(function()
						loader():replace(dep, component({ name = dep, origin = origins[dep] })):await()
					end)
					if not dep_ok then
						failures[#failures + 1] = "plugin " .. dep .. " failed: " .. tostring(dep_err)
						components[dep] = dep_prev
						if stateOf(dep) == "ACTIVE" then
							restored[#restored + 1] = dep
						end
					end
				end
			end
		end
		if name and running then
			local mod_deps = prims.module_dependents and prims.module_dependents(name) or {}
			local before_seed = { [name] = true }
			for _, dep in ipairs(mod_deps) do
				before_seed[dep] = true
			end
			emit_before_reload(before_seed)
			if prims.sync_core_modules then
				prims.sync_core_modules()
			end
			local prev = components[name]
			local ok, err = pcall(function()
				loader():replace(name, component({ name = name, origin = origins[name] })):await()
			end)
			if not ok then
				failures[#failures + 1] = "plugin " .. name .. " failed: " .. tostring(err)
				-- loader:replace has put the old definition back; Rust restores it too
				components[name] = prev
				if stateOf(name) == "ACTIVE" then
					restored[#restored + 1] = name
				end
			else
				rebuild_mod_deps(mod_deps)
			end
		else
			-- which rows restart. A user's `/reload` (Rust passes `targets`) restarts every target,
			-- as on the old path: a built-in that reads files in `apply` (themes, prompts, skills,
			-- agent definitions) picks up edited files. A rows-driven reload (config change,
			-- Kernel::apply_rows_report: no targets) restarts only the rows whose definition
			-- fingerprint changed (`row.fingerprint`: the definition source and its lua/ modules,
			-- rows.rs), as Cordis' HMR does; config changes are applied by the loader itself.
			local restart = {}
			if targets ~= nil then
				restart = targeted
			else
				for _, row in ipairs(list) do
					local prior = old[row.id]
					if prior and prior.fingerprint ~= row.fingerprint then
						restart[row.id] = true
					end
				end
			end
			-- Named reload of a currently non-running plugin still restarts active dependents that
			-- previously `require`d modules from it once the target starts.
			local mod_deps = {}
			if name and prims.module_dependents then
				for _, row in ipairs(list) do
					if row.id == name and not row.disabled then
						mod_deps = prims.module_dependents(name) or {}
						break
					end
				end
			end
			-- one at a time, last row first: first the rows that are gone (the old path unloaded
			-- them before reloading), then the targets
			local current = {}
			for _, row in ipairs(loader().root.data) do
				current[#current + 1] = row
			end
			local present = {}
			for _, row in ipairs(list) do
				present[row.id] = true
			end
			local gone_seed = {}
			for _, row in ipairs(current) do
				if not present[row.id] then
					gone_seed[row.id] = true
				end
			end
			for id, v in pairs(restart) do
				if v then
					gone_seed[id] = true
				end
			end
			for _, dep in ipairs(mod_deps) do
				if dep ~= name and pre[dep] == "ACTIVE" then
					gone_seed[dep] = true
				end
			end
			for _, row in ipairs(list) do
				local prior = old[row.id]
				if prior and (prior.config ~= row.config or prior.disabled ~= row.disabled) then
					gone_seed[row.id] = true
				end
			end
			emit_before_reload(gone_seed)
			-- the picked rows leave one at a time, last row first (the loader disposes each and
			-- waits before the next; cordis unloads dependents before their providers); returns them
			local function leave(pick)
				local ids = {}
				for i = #current, 1, -1 do
					if pick(current[i].id) then
						ids[#ids + 1] = current[i].id
						table.remove(current, i)
					end
				end
				if ids[1] then
					loader():remove_rows(ids):await()
				end
				return ids
			end
			-- what a failed restart puts back: the component a row that was already in the tree
			-- last ran (a new row has none, whatever an earlier plugin of that name left)
			local prev = {}
			for id in pairs(restart) do
				if old[id] then
					prev[id] = components[id]
				end
			end
			local gone = leave(function(id)
				return not present[id]
			end)
			leave(function(id)
				return restart[id]
			end)
			if prims.sync_core_modules then
				prims.sync_core_modules()
			end
			loader():apply_rows(list):await()
			releaseRows(list, gone)
			-- a restarted plugin whose new definition fails (FAILED) runs on its previous one again
			-- . One left PENDING
			-- waits for a service nobody provides: Cordis semantics, not a failure; it stays and is
			-- reported in `pending` with what it waits for (an inject cycle never gets here: the
			-- host refuses it before the switch). Rows are taken in order: putting one back can let
			-- a later one start on its own.
			-- Putting a row back can start one that waited for it (a dependent restarts once its
			-- provider is back, asynchronously, or was already passed over while it waited): after
			-- each restore the tree settles and the states are read again, and the rows are scanned
			-- again while a restore was made; each row is put back at most once.
			local tried = {}
			local now, itemErrors = states()
			local again = true
			while again do
				again = false
				for _, row in ipairs(list) do
					local id = row.id
					if restart[id] and not tried[id] and now[id] == "FAILED" and prev[id] and not row.disabled then
						tried[id] = true
						again = true
						local err = components_of.errors[id] or itemErrors[id]
						failures[#failures + 1] = "plugin " .. id .. " failed" .. (err and (": " .. err) or "")
						pcall(function()
							loader():replace(id, prev[id]):await()
						end)
						loader():await():await()
						now, itemErrors = states()
						if now[id] == "ACTIVE" then
							components[id] = prev[id]
							restored[#restored + 1] = id
							-- the fingerprint it ran under before this reload: the next rows-driven
							-- reload sees the new definition as changed and retries it
							row.fingerprint = old[id] and old[id].fingerprint
						end
					end
				end
			end
			if name and #mod_deps > 0 then
				local target_state = stateOf(name)
				if target_state == "ACTIVE" or target_state == "PENDING" then
					rebuild_mod_deps(mod_deps)
				end
			end
		end
		-- the restarts this reload set off (a dependent of a replaced provider restarts after the
		-- replace returns) have settled before anything is reported
		loader():await():await()
		-- one dump serves the report (what waits, what failed, what runs)
		local dump = loader():dump(DUMP_NO_EFFECTS)
		pending = booted.pending(dump)
		for _, f in ipairs(failed(dump)) do
			failures[#failures + 1] = f
		end
		-- applied: every plugin that applied again during this reload and runs now (a restarted
		-- dependent included), in row order; a plugin put back on a definition it already ran
		-- counts only when it was re-enabled (`restored`)
		local keptSet = {}
		for _, id in ipairs(kept) do
			keptSet[id] = true
		end
		local applied = {}
		local final = states(dump)
		for _, row in ipairs(list) do
			local id = row.id
			-- a row in the tree that never applied and was not waiting (it failed its first apply,
			-- or never imported) and starts now is not "reloaded"; one that waited for a service
			-- (PENDING before this reload) and runs now did apply during it
			local firstStart = old[id] ~= nil and before[id] == nil and pre[id] ~= "PENDING"
			if
				not keptSet[id]
				and not firstStart
				and (components_of.applies[id] or 0) > (before[id] or 0)
				and final[id] == "ACTIVE"
			then
				applied[#applied + 1] = id
			end
		end
		return { applied = applied, failures = failures, restored = restored, kept = kept, pending = pending }
	end
	-- start and reload run in the host's own context (prims.run_root), so the fibers they
	-- build capture the root scope, never the grant of whoever triggered the reload
	local function inRoot(fn)
		return function(...)
			if prims.run_root then
				return prims.run_root(fn, ...)
			end
			return fn(...)
		end
	end
	booted.start = inRoot(start)
	booted.reload = inRoot(reload)

	return booted
end
