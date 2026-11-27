-- mare.Service: the
-- published value stays a traceable cordis Service, so `self.ctx` is the caller on every read
-- path; declared methods run their guard around the implementation as the provider's owner;
-- admission happens before publishing, export lives as an effect of the provider's fiber.
local describe = mare.test.describe
local it = mare.test.it
local expect = mare.test.expect

local booted = require("mare_kernel.test").boot()
local cordis, host = booted.cordis, booted.host
local makeService = require("mare_kernel.service")

-- stub prims: a contract per name ({ methods = {...}, required = {...}, typed = {...}, full? }:
-- full unless `full = false`, a name-only one, which has no method guard), the owner primitive as
-- a plain variable, the session barrier and a log of what the guards and exports saw. The barrier
-- has the kernel's rule (lib.rs `check`): once the session is poisoned (`poison(reason, guarded)`),
-- a method of a service it guards is refused unless that service allows it; a service it does not
-- guard always passes. `guarded` = { [service] = { [allowed method] = true } }. `barrier.calls`
-- records the (service, method) it was asked about.
local function stubPrims(contracts)
	local log, current, poisoned, guarded = {}, nil, nil, {}
	local prims = { owner = {}, contracts = {} }
	local calls = {}
	prims.barrier = setmetatable({ calls = calls }, {
		__call = function(_, service, method)
			calls[#calls + 1] = tostring(service) .. "." .. tostring(method)
			local allowed = guarded[service]
			if poisoned and allowed and not allowed[method] then
				error(poisoned, 0)
			end
		end,
	})
	function prims.poison(reason, services)
		poisoned, guarded = reason, services
	end
	function prims.owner.current()
		return current
	end
	function prims.owner.enter(id, fn, ...)
		local saved = current
		current = id
		local r = table.pack(pcall(fn, ...))
		current = saved
		if not r[1] then
			error(r[2], 0)
		end
		return table.unpack(r, 2, r.n)
	end
	function prims.owner.enter_sync(id, fn, ...)
		local saved = current
		current = id
		local r = table.pack(pcall(fn, ...))
		current = saved
		if not r[1] then
			error(r[2], 0)
		end
		return table.unpack(r, 2, r.n)
	end
	-- mare.Service hands its class to the kernel once (the admission walk's stop); nothing to do here
	function prims.contracts.base() end
	function prims.contracts.admit(name, value)
		local c = contracts[name]
		if not c then
			error({ code = "invalid_arguments", message = "service `" .. name .. "` has no contract" }, 0)
		end
		for _, m in ipairs(c.required or {}) do
			if type(value[m]) ~= "function" then
				error({ code = "invalid_arguments", message = name .. " requires method `" .. m .. "`" }, 0)
			end
		end
		return { name = name, contract = c }
	end
	-- no registration, host-provided, or remote-only methods in these specs; nothing calls through the JSON path
	function prims.contracts.registrations()
		return {}
	end
	function prims.contracts.host_methods()
		return {}
	end
	function prims.contracts.host_remote_methods()
		return {}
	end
	function prims.contracts.remote_methods()
		return {}
	end
	function prims.contracts.take_export()
		return false
	end
	function prims.contracts.methods(handle)
		return handle.contract.methods
	end
	function prims.contracts.guard_handle(handle)
		return { kind = "guard_handle", name = handle.name, contract = handle.contract }
	end
	function prims.contracts.method_guard(guard_handle, method)
		assert(guard_handle and guard_handle.kind == "guard_handle", "method_guard requires a GuardHandle")
		return {
			passthrough = guard_handle.contract.passthrough and guard_handle.contract.passthrough[method] or false,
			enter = function(...)
				log[#log + 1] = "enter " .. method .. " " .. table.concat({ ... }, ",")
				if (...) == "deny" then
					return { code = "denied", message = guard_handle.name .. "." .. method .. " is not allowed" }
				end
			end,
			leave = function(packed)
				if guard_handle.contract.wrap_leave then
					return { checked = packed[1] }
				end
				return packed[1]
			end,
			fail = function(err)
				return { code = "internal", message = tostring(err) }
			end,
		}
	end
	function prims.contracts.export(handle)
		log[#log + 1] = "export " .. handle.name
		return 7
	end
	function prims.contracts.withdraw(owner, seq)
		log[#log + 1] = "withdraw " .. tostring(owner) .. ":" .. tostring(seq)
	end
	return prims, log
end

describe("mare.Service", function()
	it("publishes a traceable service: self.ctx is the caller through property, get and inject", function()
		local prims = stubPrims({ ms_echo = { methods = { "who" } } })
		local MareService = makeService(prims)
		local Echo = cordis.class("Echo", MareService)
		function Echo:constructor(ctx)
			Echo.super.constructor(self, ctx, "ms_echo")
		end
		function Echo:who()
			return self.ctx
		end
		local root = cordis.Context.new({ host = host })
		cordis.await(root:use({
			name = "provider",
			apply = function(ctx)
				Echo.new(ctx)
			end,
		}))
		local seen = {}
		cordis.await(root:use({
			name = "consumer",
			inject = { "ms_echo" },
			apply = function(ctx)
				seen.prop = { ctx.ms_echo:who(), ctx }
				seen.get = { ctx:get("ms_echo"):who(), ctx }
				ctx:inject({ "ms_echo" }, function(sub)
					seen.inject = { sub.ms_echo:who(), sub }
				end)
			end,
		}))
		cordis.tick(host)
		for _, path in ipairs({ "prop", "get", "inject" }) do
			expect.eq(seen[path] ~= nil, true)
			expect.eq(rawequal(seen[path][1], seen[path][2]), true)
		end
		expect.eq(cordis.Service.is(root:get("ms_echo")), true)
	end)

	it("runs the guard around a declared method as the provider's owner", function()
		local prims, log = stubPrims({
			ms_g = { methods = { "op", "typed_op" }, passthrough = { op = true }, wrap_leave = true },
		})
		local G = cordis.class("G", makeService(prims))
		function G:constructor(ctx)
			G.super.constructor(self, ctx, "ms_g")
		end
		-- one return value: a traced method passes only the first one on (cordis-lua trace.lua
		-- createShadowMethod; reported to cordis-core)
		function G:op(a)
			return a .. ":" .. tostring(prims.owner.current())
		end
		function G:typed_op(a)
			return a * 2
		end
		function G:helper()
			return prims.owner.current()
		end
		local root = cordis.Context.new({ host = host })
		-- the loader runs apply as the plugin's owner (prims.owner.enter)
		cordis.await(root:use({
			name = "provider",
			apply = function(ctx)
				prims.owner.enter(42, G.new, ctx)
			end,
		}))
		local svc = root:get("ms_g")
		expect.eq(svc:op("x"), "x:42") -- the implementation runs as the provider
		expect.eq(svc:typed_op(3).checked, 6) -- a checked result goes through leave
		expect.eq(svc:helper(), nil) -- undeclared methods are not wrapped
		local ok, err = pcall(svc.op, svc, "deny")
		expect.eq(ok, false)
		expect.eq(err.code, "denied")
		expect.eq(log[2], "enter op x")
	end)

	it("refuses a method of a poisoned session before its implementation runs", function()
		local prims = stubPrims({
			ms_full = { methods = { "op" } },
			ms_diag = { methods = { "op", "info" } },
		})
		-- what ran (recorded apart from the owner it ran as: a nil owner must not erase the record)
		local ran, owner = {}, {}
		local function define(name)
			local C = cordis.class(name, makeService(prims))
			function C:constructor(ctx)
				C.super.constructor(self, ctx, name)
			end
			function C:op()
				ran[#ran + 1] = name .. ".op"
				owner[name] = prims.owner.current() or false
				return "ok"
			end
			function C:info()
				ran[#ran + 1] = name .. ".info"
				return "diagnostic"
			end
			return C
		end
		local Full, Diag = define("ms_full"), define("ms_diag")
		local root = cordis.Context.new({ host = host })
		cordis.await(root:use({
			name = "provider",
			apply = function(ctx)
				prims.owner.enter(42, function()
					Full.new(ctx)
					Diag.new(ctx)
				end)
			end,
		}))
		local full, diag = root:get("ms_full"), root:get("ms_diag")
		-- unpoisoned: both run as the provider; the barrier is asked about each (service, method)
		expect.eq(full:op(), "ok")
		expect.eq(diag:op(), "ok")
		expect.eq(table.concat(ran, ","), "ms_full.op,ms_diag.op")
		expect.eq(owner.ms_full, 42)
		expect.eq(owner.ms_diag, 42)
		expect.eq(table.concat(prims.barrier.calls, ","), "ms_full.op,ms_diag.op")
		ran = {}
		-- poisoned: both services guarded, ms_diag allows `info` (a diagnostic)
		prims.poison("session transaction failed: disk full", { ms_full = {}, ms_diag = { info = true } })
		for _, svc in ipairs({ full, diag }) do
			local ok, err = pcall(svc.op, svc)
			expect.eq(ok, false)
			expect.eq(tostring(err), "session transaction failed: disk full")
		end
		expect.eq(#ran, 0) -- neither implementation ran
		expect.eq(diag:info(), "diagnostic") -- an allowed method still runs
		expect.eq(table.concat(ran, ","), "ms_diag.info")
	end)

	it("refuses before publishing and ties the export to the provider's fiber", function()
		local prims, log = stubPrims({ ms_req = { methods = { "must" }, required = { "must" } } })
		local MareService = makeService(prims)
		local Missing = cordis.class("Missing", MareService)
		function Missing:constructor(ctx)
			Missing.super.constructor(self, ctx, "ms_req")
		end
		local root = cordis.Context.new({ host = host })
		local fiber = root:use({
			name = "bad",
			apply = function(ctx)
				Missing.new(ctx)
			end,
		})
		local ok = pcall(cordis.await, fiber)
		expect.eq(ok, false)
		expect.eq(fiber.state, cordis.FiberState.FAILED)
		expect.eq(root:get("ms_req"), nil)

		local Good = cordis.class("Good", MareService)
		function Good:constructor(ctx)
			Good.super.constructor(self, ctx, "ms_req")
		end
		function Good:must() end
		local good = root:use({
			name = "good",
			apply = function(ctx)
				prims.owner.enter(5, Good.new, ctx)
			end,
		})
		cordis.await(good)
		expect.eq(log[#log], "export ms_req")
		cordis.await(good:dispose())
		expect.eq(log[#log], "withdraw 5:7")
		expect.eq(root:get("ms_req"), nil)
	end)

	it("materializes per-method guards lazily on first invocation and caches across cached references", function()
		local prims = stubPrims({
			ms_lazy = { methods = { "hot", "cold" }, wrap_leave = true },
		})
		local guard_calls = {}
		local orig_guard = prims.contracts.method_guard
		function prims.contracts.method_guard(handle, method)
			guard_calls[method] = (guard_calls[method] or 0) + 1
			return orig_guard(handle, method)
		end

		local Lazy = cordis.class("Lazy", makeService(prims))
		function Lazy:constructor(ctx)
			Lazy.super.constructor(self, ctx, "ms_lazy")
		end
		function Lazy:hot(x)
			return x + 1
		end
		function Lazy:cold(x)
			return x + 2
		end

		local root = cordis.Context.new({ host = host })
		local fiber = root:use({
			name = "lazy_provider",
			apply = function(ctx)
				prims.owner.enter(9, Lazy.new, ctx)
			end,
		})
		cordis.await(fiber)

		-- Construction must not call `method_guard` for any method.
		expect.eq(guard_calls.hot, nil)
		expect.eq(guard_calls.cold, nil)

		local svc = root:get("ms_lazy")
		-- Capture a method reference before first call and verify identity stays stable across
		-- lazy materialization (`mare_lib.service` relies on `rawget(svc, k)` identity).
		local raw_before = rawget(fiber.provisions:values()[1].value, "hot")
		local cached_hot = svc.hot
		expect.eq(cached_hot(svc, 10).checked, 11)
		expect.eq(rawequal(rawget(fiber.provisions:values()[1].value, "hot"), raw_before), true)
		expect.eq(cached_hot(svc, 20).checked, 21)
		expect.eq(svc:hot(30).checked, 31)
		expect.eq(guard_calls.hot, 1, "hot method_guard must be built exactly once")
		expect.eq(guard_calls.cold, nil, "untouched cold method must never call method_guard")

		-- Withdrawing before `cold` is ever called must still refuse `cold` with `unavailable`.
		cordis.await(fiber:dispose())
		local ok, err = pcall(svc.cold, svc, 1)
		expect.eq(ok, false)
		expect.eq(err.code, "unavailable")
	end)
end)
