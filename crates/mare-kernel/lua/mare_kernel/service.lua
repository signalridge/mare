-- mare.Service: the base class of every mare
-- service. It is a cordis.Service, so the instance it publishes is traceable: whoever reads it
-- (`ctx.<name>`, `ctx:get`, an `inject` callback, the RPC bridge) gets Cordis' tracking proxy and
-- `self.ctx` inside a method is the caller's ctx. The contract is enforced without an outer proxy:
--
--   * admission: before publishing, the contract is selected and checked (`prims.contracts.admit`;
--     a missing required method, an undeclared function, or no contract raises, so the plugin's
--     fiber is FAILED);
--   * guards: every method the contract declares is replaced on the instance by a wrapper that runs
--     the method's guard (typed argument / result checks, error values) around the implementation;
--     the wrapper switches the owner (attribution, not rights) to the provider's fiber, so events
--     emitted inside a method are the provider's; the tool context stays the caller's;
--   * export: in an effect of the provider's fiber, the admitted value is registered for JSON callers
--     and the catalog, and withdrawn with the fiber.
--
-- `prims` is the boot fragment's capability table (types/mare/host/*.lua): `contracts` and
-- `owner`. boot.lua builds the class once: `require("mare_kernel.service")(prims)`.
return function(prims)
	local cordis = require("cordis")
	local calls = require("mare_kernel.calls")

	local Service = cordis.class("mare.Service", cordis.Service)
	prims.contracts.base(Service)
	-- Per-root override table for isolated `mare.test.host()` trees: routes `mare.Service`
	-- contract admission/export/barriers to that test host's kernel without touching the global class.
	local root_prims = setmetatable({}, { __mode = "k" })
	local function bind_root(root_ctx, custom_prims)
		root_prims[root_ctx] = custom_prims
	end

	local function refuse_stale(svc, name, method, live, registration)
		if registration then
			local fiber = calls.caller_fiber(svc.ctx)
			if fiber and calls.inactive(fiber) then
				error(calls.coded("inactive_ctx", ("%s.%s: the caller's context was disposed"):format(name, method)), 3)
			end
		end
		if not live.alive then
			error(
				calls.coded("unavailable", ("service `%s` was withdrawn; this view is no longer valid"):format(name)),
				3
			)
		end
	end

	-- the wrapper of one declared method: session barrier, guard, owner switch, implementation,
	-- result. A poisoned session refuses the method before anything runs (prims.barrier: agent /
	-- session / tools, diagnostics allowed)
	-- `live`: this instance's provision ({ alive }); a reference kept past its withdrawal is refused
	-- (`unavailable`), even once the same class is provided again.
	-- `registration`: what the method registers is an effect on the caller's fiber, so a caller
	-- whose context has gone away is refused before the provider runs (`inactive_ctx`)
	local function guarded(p, guard, owner, impl, method, name, live, registration, is_host)
		guard = guard or {}
		return function(svc, ...)
			p.barrier(name, method)
			refuse_stale(svc, name, method, live, registration)
			if guard.enter then
				local err = guard.enter(...)
				if err ~= nil then
					error(err, 2)
				end
			end
			local results
			if is_host then
				local ok_fn, host_fn, is_async = pcall(p.contracts.host_method, name, method)
				if not ok_fn then
					results = table.pack(false, host_fn)
				else
					local ctx = type(svc) == "table" and svc.ctx or nil
					local fiber = calls.caller_fiber(ctx)
					if fiber and calls.disposed(fiber) then
						error(
							calls.coded(
								"inactive_ctx",
								("%s.%s: the caller's context was disposed"):format(name, method)
							),
							2
						)
					end
					local _, consumer_owner = calls.resolve_caller(fiber)
					local prev_ctx = calls.set_active_ctx(ctx)
					local restore_ctx <close> = setmetatable({}, {
						__close = function()
							calls.set_active_ctx(prev_ctx)
						end,
					})
					if is_async then
						-- Always enter `consumer_owner` (including `nil`) so an async host call
						-- invoked through a root/unowned ctx strips any outer plugin OWNER.
						local ok, inner = pcall(p.owner.enter, consumer_owner, function(...)
							return table.pack(pcall(host_fn, ...))
						end, ...)
						results = ok and inner or table.pack(false, inner)
					else
						results = table.pack(pcall(p.owner.enter_sync, consumer_owner, host_fn, ...))
					end
				end
			elseif owner ~= nil then
				-- caught inside the owner switch: a raised value (a coded table) crossing the Rust
				-- call would arrive as its string
				local ok, inner = pcall(p.owner.enter, owner, function(...)
					return table.pack(pcall(impl, ...))
				end, svc, ...)
				results = ok and inner or table.pack(false, inner)
			else
				results = table.pack(pcall(impl, svc, ...))
			end
			if not results[1] then
				error(guard.fail and guard.fail(results[2]) or results[2], 2)
			end
			if guard.passthrough then
				return table.unpack(results, 2, results.n)
			end
			local value, failure = guard.leave(table.pack(table.unpack(results, 2, results.n)))
			if failure ~= nil then
				error(failure, 2)
			end
			if guard.keep_extra and results.n > 2 then
				return value, table.unpack(results, 3, results.n)
			end
			return value
		end,
			method
	end

	--- `name`: the service name its contract declares (default: the class's static `provide`);
	--- `options`: what a provide takes (`replace`, …), handed to admission.
	function Service:constructor(ctx, name, options)
		name = name or self.provide
		assert(type(name) == "string", "mare.Service needs the service name")
		local root = type(ctx) == "table" and ctx[cordis.Context.root] or nil
		local p = (root and root_prims[root]) or prims
		calls.refuse_taken(ctx, name, 2)
		local handle = p.contracts.admit(name, self, options)
		calls.admitted[self] = name
		local owner = p.owner.current()
		local live = { alive = true }
		local guard_handle = p.contracts.guard_handle(handle)
		local registers = {}
		for _, method in ipairs(p.contracts.registrations(handle)) do
			registers[method] = true
		end
		-- Defer `prims.contracts.method_guard` FFI and per-method guard construction until the
		-- method is actually invoked, keeping the published function identity stable on `self`
		-- (required by `mare_lib.service` monkey-patch detection and `cordis.trace` method caching)
		-- and tail-calling so error stack levels behave identically.
		local function install_lazy(method, impl, is_host)
			local registration = not is_host and registers[method] or false
			local fn
			rawset(self, method, function(svc, ...)
				if not fn then
					local guard = p.contracts.method_guard(guard_handle, method)
					fn = guarded(
						p,
						guard,
						(not is_host) and owner or nil,
						impl,
						method,
						name,
						live,
						registration,
						is_host
					)
				end
				return fn(svc, ...)
			end)
		end
		for _, method in ipairs(p.contracts.methods(handle)) do
			local impl = self[method]
			if type(impl) == "function" then
				install_lazy(method, impl, false)
			end
		end
		for _, method in ipairs(p.contracts.host_methods(handle)) do
			install_lazy(method, nil, true)
		end
		Service.super.constructor(self, ctx, name)
		ctx:effect(function()
			-- live while exported (the admission handle exports once: one run of this effect)
			local seq = p.contracts.export(handle, self)
			live.alive = true
			return function()
				live.alive = false
				p.contracts.withdraw(owner, seq)
			end
		end, "mare.Service export " .. name)
	end

	return Service, bind_root
end
