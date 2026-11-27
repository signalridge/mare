-- Isolated real-host test fixture behind `mare.test.host(opts?)`.
-- Runs the same Cordis tree, MareLoader, event bus, policy, hooks, and `mare.Service` contract
-- admission as a production host, isolated to its own root context and kernel state so tests
-- cannot interfere with the outer test runner or with each other.
return function(outer, test_prims, helpers, opts)
	local cordis = outer.cordis
	local host = outer.host
	local Service = outer.Service
	local calls = require("mare_kernel.calls")
	local proto = require("cordis.proto")

	local root = cordis.Context.new({ host = host })
	outer.bind_service_root(root, test_prims)

	-- Enforce `mare.Service` admission for every declared service on this isolated tree.
	root:on("internal/provide", function(_, name, value)
		if type(name) ~= "string" then
			return
		end
		local raw = value
		while proto.isProxy(raw) do
			raw = proto.proxyTarget(raw)
		end
		if calls.admitted[raw] == name then
			return
		end
		if test_prims.contracts.declared(name) then
			error(
				calls.coded(
					"invalid_arguments",
					("service `%s` is declared: provide it through mare.Service (mare_lib.service)"):format(name)
				),
				0
			)
		end
	end, { global = true })

	local bus = require("mare_kernel.bus")(root, test_prims)
	local task_fiber = root:use(require("mare_kernel.plugins.task"), { prims = test_prims })
	local bus_fiber = root:use(require("mare_kernel.plugins.bus"), { prims = test_prims, bus = bus })

	-- Enforce event declarations on direct `ctx:emit`/`bail`/`waterfall`/`parallel`/`serial`,
	-- mirroring `mare_kernel/boot.lua`.
	if test_prims.events and test_prims.events.check_dispatch then
		local check_dispatch = test_prims.events.check_dispatch
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

	local kernel_fiber = root:use(require("mare_kernel.plugins.kernel")(test_prims, Service))
	local policy_fiber = root:use(require("mare_kernel.plugins.policy")(test_prims, Service, bus))
	local hooks_fiber = root:use(require("mare_kernel.plugins.hooks")(test_prims, Service))

	local row_fibers = setmetatable({}, { __mode = "k" })
	local components_of = require("mare_kernel.component")(test_prims, root, row_fibers)

	local defs = {}
	local origins = {}
	local rows_by_id = {}
	local row_order = {}
	local provided_fibers = {}
	local keeper = helpers.keeper
	local disposed = false

	local function ensure_alive(method)
		if disposed then
			error("mare.TestHost:" .. method .. "() cannot be called after dispose()", 3)
		end
	end

	local function shallow_copy(tbl)
		local out = {}
		if type(tbl) == "table" then
			for k, v in pairs(tbl) do
				out[k] = v
			end
		end
		return out
	end

	local function merge_defaults(def, cfg)
		local merged = shallow_copy(cfg)
		if type(def) == "table" and type(def.config_schema) == "table" then
			for key, spec in pairs(def.config_schema) do
				if merged[key] == nil and type(spec) == "table" and spec.default ~= nil then
					merged[key] = spec.default
				end
			end
		end
		return merged
	end

	local function build_component(name)
		local def = defs[name]
		if not def then
			error("module '" .. name .. "' not found:", 0)
		end
		return components_of.component(def, origins[name] or "user"), origins[name] or "user", false
	end

	local loader_fiber = root:use(require("mare_kernel.plugins.loader"), {
		prims = {
			refused = function()
				return {}
			end,
			rows = function()
				local list = {}
				for _, id in ipairs(row_order) do
					list[#list + 1] = rows_by_id[id]
				end
				return list
			end,
		},
		component = build_component,
	})

	cordis.await(task_fiber)
	cordis.await(bus_fiber)
	cordis.await(kernel_fiber)
	cordis.await(policy_fiber)
	cordis.await(hooks_fiber)
	cordis.await(loader_fiber)

	local loader = root.reflect:get("loader", false)

	local function current_rows()
		local list = {}
		for _, id in ipairs(row_order) do
			list[#list + 1] = rows_by_id[id]
		end
		return list
	end

	local function dump_by_id()
		local out = {}
		for _, item in ipairs(loader:dump({ effects = false })) do
			if not item.refused then
				out[item.id] = item
			end
		end
		return out
	end

	local function register_def(def_or_name)
		if type(def_or_name) == "string" then
			local def = helpers.load_builtin(def_or_name)
			local name = def.name or def_or_name
			defs[name] = def
			origins[name] = "builtin"
			return name, def, "builtin"
		elseif type(def_or_name) == "table" then
			local name = helpers.register_def(def_or_name)
			defs[name] = def_or_name
			origins[name] = "user"
			return name, def_or_name, "user"
		else
			error("mare.TestHost:load(def, cfg): def must be a plugin definition table or built-in name string", 3)
		end
	end

	local TestHost = {}

	function TestHost:load(def_or_name, cfg)
		ensure_alive("load")
		if cfg ~= nil and type(cfg) ~= "table" then
			error("mare.TestHost:load(def, cfg): cfg must be a table or nil", 2)
		end
		local name, def, origin = register_def(def_or_name)
		local config = merge_defaults(def, cfg)
		if not rows_by_id[name] then
			row_order[#row_order + 1] = name
		end
		rows_by_id[name] = {
			id = name,
			name = name,
			config = config,
			origin = origin,
		}
		loader:apply_rows(current_rows()):await()
		return self
	end

	function TestHost:provide(name, impl, provide_opts)
		ensure_alive("provide")
		if type(name) ~= "string" or name == "" then
			error("mare.TestHost:provide(name, impl, opts?): name must be a non-empty string", 2)
		end
		if type(impl) ~= "table" then
			error("mare.TestHost:provide(name, impl, opts?): impl must be a table", 2)
		end
		if provide_opts ~= nil and type(provide_opts) ~= "table" then
			error("mare.TestHost:provide(name, impl, opts?): opts must be a table or nil", 2)
		end
		local FakeService = cordis.class("TestService_" .. name, Service)
		for k, v in pairs(impl) do
			FakeService[k] = v
		end
		function FakeService:constructor(ctx)
			local owner = test_prims.owner.open("test:provide:" .. name, "user")
			ctx:effect(function()
				return function()
					test_prims.owner.close(owner)
				end
			end, "owner")
			test_prims.owner.enter(owner, FakeService.super.constructor, self, ctx, name, provide_opts)
		end
		local fiber = root:use(FakeService)
		local ok, err = pcall(cordis.await, fiber)
		if not ok then
			pcall(function()
				cordis.await(fiber:dispose())
			end)
			error(err, 2)
		end
		provided_fibers[fiber] = true
		loader:apply_rows(current_rows()):await()
		return function()
			if disposed or not provided_fibers[fiber] then
				return
			end
			provided_fibers[fiber] = nil
			if fiber.state ~= cordis.FiberState.DISPOSED then
				cordis.await(fiber:dispose())
			end
			if not disposed then
				loader:apply_rows(current_rows()):await()
			end
		end
	end

	function TestHost:get(name)
		if disposed then
			return nil
		end
		if type(name) ~= "string" or name == "" then
			error("mare.TestHost:get(name): name must be a non-empty string", 2)
		end
		return root.reflect:get(name, false)
	end

	function TestHost:reload(name, new_def, new_cfg)
		ensure_alive("reload")
		if type(name) ~= "string" or name == "" then
			error("mare.TestHost:reload(name, def?, cfg?): name must be a non-empty string", 2)
		end
		if not rows_by_id[name] then
			return false, "plugin `" .. name .. "` is not loaded in this test host"
		end
		if new_cfg ~= nil and type(new_cfg) ~= "table" then
			error("mare.TestHost:reload(name, def?, cfg?): cfg must be a table or nil", 2)
		end
		local prev_def = defs[name]
		local prev_origin = origins[name]
		local prev_row = rows_by_id[name]
		local target_def = prev_def
		if new_def ~= nil then
			local reg_ok, reg_name, def_tbl, reg_origin = pcall(register_def, new_def)
			if not reg_ok then
				return false, tostring(reg_name)
			end
			if reg_name ~= name then
				defs[name] = prev_def
				origins[name] = prev_origin
				return false,
					("reload target `%s` does not match definition name `%s`"):format(name, tostring(reg_name))
			end
			target_def = def_tbl
			origins[name] = reg_origin
		elseif prev_origin == "builtin" then
			local reg_ok, _, def_tbl = pcall(register_def, name)
			if not reg_ok then
				return false, tostring(def_tbl)
			end
			target_def = def_tbl
		end
		if new_cfg ~= nil then
			rows_by_id[name] = {
				id = name,
				name = name,
				config = merge_defaults(target_def, new_cfg),
				origin = origins[name] or prev_origin,
			}
		end
		local item = dump_by_id()[name]
		local running = item and item.state ~= nil and item.state ~= "NONE" and not item.disabled
		if running and new_cfg == nil then
			local comp = components_of.component(target_def, origins[name] or "user")
			local ok, err = pcall(function()
				loader:replace(name, comp):await()
			end)
			if not ok then
				defs[name] = prev_def
				origins[name] = prev_origin
				rows_by_id[name] = prev_row
				if prev_origin == "user" and type(prev_def) == "table" then
					pcall(helpers.register_def, prev_def)
				end
				return false, calls.error_message(err)
			end
			return true
		end
		loader:apply_rows(current_rows()):await()
		local after = dump_by_id()[name]
		if after and after.state == "FAILED" then
			local err_msg = components_of.errors[name] or after.error or "plugin failed"
			defs[name] = prev_def
			origins[name] = prev_origin
			rows_by_id[name] = prev_row
			if prev_origin == "user" and type(prev_def) == "table" then
				pcall(helpers.register_def, prev_def)
			end
			loader:apply_rows(current_rows()):await()
			return false, err_msg
		end
		return true
	end

	function TestHost:state(name)
		if disposed then
			return rows_by_id[name] and "DISPOSED" or nil
		end
		if type(name) ~= "string" or name == "" then
			error("mare.TestHost:state(name): name must be a non-empty string", 2)
		end
		local item = dump_by_id()[name]
		return item and item.state or nil
	end

	function TestHost:error(name)
		if type(name) ~= "string" or name == "" then
			error("mare.TestHost:error(name): name must be a non-empty string", 2)
		end
		if components_of.errors[name] then
			return components_of.errors[name]
		end
		if not disposed then
			local item = dump_by_id()[name]
			if item and item.error then
				return item.error
			end
		end
		return nil
	end

	function TestHost:_settled_problems()
		local problems = {}
		if disposed then
			for fiber, name in pairs(row_fibers) do
				if fiber.state ~= cordis.FiberState.DISPOSED then
					problems[#problems + 1] = ("plugin %s is still %s after dispose"):format(
						name,
						tostring(fiber.state)
					)
				end
			end
			return problems
		end
		for _, id in ipairs(row_order) do
			local item = dump_by_id()[id]
			local st = item and item.state or "NONE"
			if st ~= "ACTIVE" then
				if st == "FAILED" then
					local err = components_of.errors[id] or (item and item.error) or "failed"
					problems[#problems + 1] = ("plugin %s is FAILED: %s"):format(id, tostring(err))
				elseif st == "PENDING" then
					local missing = item and item.missing
					local waits = (missing and #missing > 0) and table.concat(missing, ", ") or "a service"
					problems[#problems + 1] = ("plugin %s is PENDING (waiting for %s)"):format(id, waits)
				else
					problems[#problems + 1] = ("plugin %s is %s"):format(id, tostring(st))
				end
			end
		end
		return problems
	end

	function TestHost:dispose()
		if disposed then
			return
		end
		disposed = true
		for fiber in pairs(provided_fibers) do
			provided_fibers[fiber] = nil
			if fiber.state ~= cordis.FiberState.DISPOSED then
				pcall(function()
					cordis.await(fiber:dispose())
				end)
			end
		end
		pcall(function()
			cordis.await(root:close())
		end)
		outer.bind_service_root(root, nil)
		keeper = nil
		local _ = keeper
	end

	local handle = setmetatable({}, { __index = TestHost })

	if opts ~= nil then
		if type(opts) ~= "table" then
			handle:dispose()
			error("mare.test.host(opts?): opts must be a table or nil", 2)
		end
		local cfg_map = opts.config or {}
		if type(cfg_map) ~= "table" then
			handle:dispose()
			error("mare.test.host(opts?): opts.config must be a table or nil", 2)
		end
		if opts.builtins ~= nil then
			if type(opts.builtins) ~= "table" then
				handle:dispose()
				error("mare.test.host(opts?): opts.builtins must be a string[] or nil", 2)
			end
			for _, builtin_name in ipairs(opts.builtins) do
				local ok, err = pcall(handle.load, handle, builtin_name, cfg_map[builtin_name])
				if not ok then
					handle:dispose()
					error(err, 2)
				end
			end
		end
	end

	return handle
end
