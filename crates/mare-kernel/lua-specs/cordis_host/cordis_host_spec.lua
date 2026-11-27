-- The cordis host on mare.task:
-- the library on the real mlua executor. Moved from mare-plugins feat/cordis-vendor
-- (tests/specs/cordis_spec.lua), adapted to the kernel's one root.
local describe = mare.test.describe
local it = mare.test.it
local expect = mare.test.expect

local booted = require("mare_kernel.test").boot()
local cordis, host, root = booted.cordis, booted.host, booted.root
local FiberState = cordis.FiberState

describe("cordis host on mare.task", function()
	it("boots one root per VM", function()
		expect.eq(require("mare_kernel.test").boot(), booted)
		expect.eq(cordis.Context.is(root), true)
		expect.eq(root.fiber.state, FiberState.ACTIVE)
	end)

	it("starts a dependent only after the component that yields provides", function()
		local order = {}
		local provider = root:use(function(ctx)
			order[#order + 1] = "provider start"
			mare.task.sleep(5) -- natively, in the component's own task
			order[#order + 1] = "provider provides"
			ctx:provide("cordis_host_x", 1)
		end)
		local dependent = root:inject({ "cordis_host_x" }, function(ctx)
			order[#order + 1] = "dependent sees " .. tostring(ctx.cordis_host_x)
		end)
		order[#order + 1] = "use returned"
		cordis.await(provider)
		cordis.await(dependent)
		expect.eq(table.concat(order, " | "), "use returned | provider start | provider provides | dependent sees 1")
		expect.eq(dependent.state, FiberState.ACTIVE)
		cordis.await(provider:dispose())
		expect.eq(dependent.state, FiberState.PENDING)
		cordis.await(dependent:dispose())
	end)

	-- guarantee: yielding-cleanups-run-concurrently
	it("finishes every yielding disposer on unload", function()
		local finished = {}
		local fiber = cordis.await(root:use(function(ctx)
			for _, ms in ipairs({ 15, 5, 10 }) do
				ctx:effect(function()
					return function()
						mare.task.sleep(ms) -- natively
						finished[#finished + 1] = ms
					end
				end)
			end
		end))
		local t0 = host.now()
		cordis.await(fiber:dispose())
		-- disposers run concurrently: all three finished, shortest first
		expect.eq(table.concat(finished, ","), "5,10,15")
		expect.eq(fiber.state, FiberState.DISPOSED)
		expect.eq(host.now() - t0 >= 15, true)
	end)

	it("keeps loading and settling while a disposer blocks natively", function()
		local blocking = cordis.await(root:use(function()
			return function()
				mare.task.sleep(300)
			end
		end))
		local disposing = blocking:dispose()
		mare.task.sleep(20)
		expect.eq(blocking.state, FiberState.UNLOADING)
		local t0 = host.now()
		local loaded = cordis.await(root:use(function() end))
		local reacted = cordis.Future
			.resolve(host, 1)
			:next(function(v)
				return v + 1
			end)
			:await()
		expect.eq(loaded.state, FiberState.ACTIVE)
		expect.eq(reacted, 2)
		expect.eq(host.now() - t0 < 200, true)
		expect.eq(blocking.state, FiberState.UNLOADING)
		cordis.await(disposing)
		expect.eq(blocking.state, FiberState.DISPOSED)
		cordis.await(loaded:dispose())
	end)

	it("closes a root for good", function()
		local other = cordis.Context.new({ host = host })
		local cleaned = false
		cordis.await(other:use(function()
			return function()
				mare.task.sleep(5)
				cleaned = true
			end
		end))
		cordis.await(other:close())
		expect.eq(cleaned, true)
		expect.eq(other.fiber.state, FiberState.DISPOSED)
		expect.error(function()
			other:use(function() end)
		end)
	end)

	it("rounds fractional sleeps up so a deadline is never early", function()
		local t0 = host.now()
		host.sleep(1.2)
		expect.eq(host.now() - t0 >= 1.2, true)
		local fired
		local TimerService = require("cordis.timer")
		local timers = cordis.await(root:use(TimerService))
		local start = host.now()
		local f, resolve = cordis.Future.withResolvers(host)
		root:timeout(function()
			fired = host.now() - start
			resolve()
		end, 2.4)
		f:await()
		expect.eq(fired >= 2.4, true)
		cordis.await(timers:dispose())
	end)

	-- listeners that block natively (mare.task) and depend on each other;
	-- parallel starts every listener before waiting, matching upstream events behavior
	it("runs parallel listeners that wait on each other natively", function()
		local token = mare.task.token()
		local order = {}
		local fiber = cordis.await(root:use(function(ctx)
			ctx:on("cordis_host/probe", function()
				order[#order + 1] = "first waits"
				token:wait()
				order[#order + 1] = "first done"
			end)
			ctx:on("cordis_host/probe", function()
				order[#order + 1] = "second opens"
				mare.task.sleep(5)
				token:cancel()
			end)
		end))
		local t0 = host.now()
		local f = root:parallel("cordis_host/probe")
		expect.eq(host.now() - t0 < 5, true) -- returned before any listener finished
		f:await()
		expect.eq(table.concat(order, " | "), "first waits | second opens | first done")
		cordis.await(fiber:dispose())
	end)

	it("starts yielding effect disposers in reverse order and runs them concurrently", function()
		local started, finished = {}, {}
		local gates = { mare.task.token(), mare.task.token(), mare.task.token() }
		local fiber = cordis.await(root:use(function(ctx)
			for i = 1, 3 do
				ctx:effect(function()
					return function()
						started[#started + 1] = i
						gates[i]:wait()
						finished[#finished + 1] = i
					end
				end)
			end
		end))
		local disposing = fiber:dispose()
		while #started < 3 do
			cordis.tick(host)
		end
		expect.eq(started, { 3, 2, 1 })
		expect.eq(finished, {})
		gates[1]:cancel()
		while #finished < 1 do
			cordis.tick(host)
		end
		expect.eq(finished, { 1 })
		gates[3]:cancel()
		while #finished < 2 do
			cordis.tick(host)
		end
		expect.eq(finished, { 1, 3 })
		gates[2]:cancel()
		cordis.await(disposing)
		expect.eq(finished, { 1, 3, 2 })
		expect.eq(fiber.state, FiberState.DISPOSED)
	end)
end)
