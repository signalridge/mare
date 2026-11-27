-- The `loader` host plugin (crates/mare-kernel/lua/mare_kernel/plugins/loader.lua): rows from
-- the kernel become the plugin tree through `config.component`; apply_rows, replace and dump are
-- what the kernel calls.
local describe = mare.test.describe
local it = mare.test.it
local expect = mare.test.expect

local booted = require("mare_kernel.test").boot()
local cordis, host = booted.cordis, booted.host
local LoaderPlugin = require("mare_kernel.plugins.loader")

-- a fresh root with the loader plugin over `defs` (name -> component)
local function setup(defs, refused, record)
	local root = cordis.Context.new({ host = host })
	local function component(name)
		return defs[name] or error("module '" .. name .. "' not found:\n\tno definition", 0)
	end
	local prims = {
		refused = function()
			return refused or {}
		end,
		profile_record = record,
	}
	cordis.await(root:use(LoaderPlugin, { component = component, prims = prims }))
	return root, root:get("loader")
end

local function byId(dump)
	local out = {}
	for _, item in ipairs(dump) do
		out[item.id] = item
	end
	return out
end

describe("loader host plugin", function()
	it("turns rows into fibers and reports them in dump", function()
		local log = {}
		local defs = {
			provider = {
				name = "provider",
				apply = function(ctx, config)
					log[#log + 1] = "provider " .. tostring(config.n)
					ctx:provide("lh_svc", {})
					ctx:effect(function()
						return function() end
					end)
				end,
			},
			consumer = {
				name = "consumer",
				inject = { "lh_svc" },
				apply = function()
					log[#log + 1] = "consumer"
				end,
			},
			waiting = { name = "waiting", inject = { "lh_never" }, apply = function() end },
		}
		local _, loader = setup(defs, {
			{ id = "dup", origin = "user", refused = "provides lh_svc already provided by provider (<builtin>)" },
		})
		loader
			:apply_rows({
				{ id = "provider", name = "provider", config = { n = 1 }, origin = "builtin" },
				{ id = "consumer", name = "consumer", config = {}, origin = "user", path = "/u/consumer/init.lua" },
				{ id = "waiting", name = "waiting", config = {}, origin = "builtin" },
				{ id = "off", name = "off", disabled = true },
				{ id = "unknown", name = "unknown", config = {}, origin = "project" },
			})
			:await()
		local d = byId(loader:dump())
		expect.eq(d.provider.state, "ACTIVE")
		expect.eq(d.provider.provides[1], "lh_svc")
		expect.eq(d.provider.effects >= 1, true)
		expect.eq(d.consumer.state, "ACTIVE")
		expect.eq(d.consumer.path, "/u/consumer/init.lua")
		expect.eq(d.waiting.state, "PENDING")
		expect.eq(d.waiting.missing[1], "lh_never")
		expect.eq(d.off.disabled, true)
		expect.eq(d.off.state, nil)
		-- refused definitions are not rows: listed after them, never run (review A5)
		expect.eq(d.dup.refused, "provides lh_svc already provided by provider (<builtin>)")
		expect.eq(d.dup.disabled, true)
		expect.eq(d.dup.state, nil)
		expect.eq(d.unknown.state, "NONE")
		expect.eq(table.concat(log, ","), "provider 1,consumer")
	end)

	it("follows a new row list: config change, removal, disabling, re-enabling", function()
		local applied, disposed = {}, {}
		local function def(name)
			return {
				name = name,
				apply = function(ctx, config)
					applied[#applied + 1] = name .. ":" .. tostring(config.n)
					ctx:effect(function()
						return function()
							disposed[#disposed + 1] = name
						end
					end)
				end,
			}
		end
		local _, loader = setup({ a = def("a"), b = def("b"), c = def("c") })
		local function rows(spec)
			local out = {}
			for _, r in ipairs(spec) do
				out[#out + 1] = { id = r[1], name = r[1], config = { n = r[2] }, disabled = r[3] }
			end
			return out
		end
		loader:apply_rows(rows({ { "a", 1 }, { "b", 1 }, { "c", 1 } })):await()
		loader:apply_rows(rows({ { "a", 2 }, { "c", 1, true } })):await()
		expect.eq(table.concat(applied, ","), "a:1,b:1,c:1,a:2")
		table.sort(disposed)
		expect.eq(table.concat(disposed, ","), "a,b,c")
		local d = byId(loader:dump())
		expect.eq(d.b, nil)
		expect.eq(d.c.disabled, true)
		loader:apply_rows(rows({ { "a", 2 }, { "c", 1 } })):await()
		expect.eq(byId(loader:dump()).c.state, "ACTIVE")
		expect.eq(applied[#applied], "c:1")
	end)

	it("replaces a definition, and puts the old one back when the new one fails", function()
		local log = {}
		local v1 = {
			name = "p",
			apply = function()
				log[#log + 1] = "v1"
			end,
		}
		local v2 = {
			name = "p",
			apply = function()
				log[#log + 1] = "v2"
			end,
		}
		local bad = {
			name = "p",
			apply = function()
				error("bad definition")
			end,
		}
		local _, loader = setup({ p = v1 })
		loader:apply_rows({ { id = "p", name = "p", config = {} } }):await()
		loader:replace("p", v2):await()
		expect.eq(table.concat(log, ","), "v1,v2")
		local ok, err = pcall(function()
			loader:replace("p", bad):await()
		end)
		expect.eq(ok, false)
		expect.match(tostring(err), "bad definition")
		expect.eq(log[#log], "v2")
		expect.eq(byId(loader:dump()).p.state, "ACTIVE")
	end)
	it("waits for a removed plugin's cleanup and retries a failed plugin (review A6)", function()
		local cleaned = false
		local attempts = 0
		local defs = {
			slow = {
				name = "slow",
				apply = function(ctx)
					ctx:effect(function()
						return cordis.async(function()
							mare.task.sleep(50)
							cleaned = true
						end)
					end)
				end,
			},
			flaky = {
				name = "flaky",
				apply = function()
					attempts = attempts + 1
					if attempts == 1 then
						error("first start fails")
					end
				end,
			},
		}
		local _, loader = setup(defs)
		local rows = {
			{ id = "slow", name = "slow", config = {} },
			{ id = "flaky", name = "flaky", config = {} },
		}
		loader:apply_rows(rows):await()
		expect.eq(byId(loader:dump()).flaky.state, "FAILED")
		-- the same rows again: the failed plugin is retried; slow is removed and its cleanup awaited
		loader:apply_rows({ { id = "flaky", name = "flaky", config = {} } }):await()
		expect.eq(cleaned, true)
		expect.eq(attempts, 2)
		expect.eq(byId(loader:dump()).flaky.state, "ACTIVE")
	end)
	-- A reload must only touch the rows that
	-- changed; today's reload_all restarts unchanged plugins too
	it("leaves the other plugins alone when one row changes", function()
		local applied, disposed = {}, {}
		local function def(name)
			return {
				name = name,
				apply = function(ctx)
					applied[name] = (applied[name] or 0) + 1
					ctx:effect(function()
						return function()
							disposed[name] = (disposed[name] or 0) + 1
						end
					end)
				end,
			}
		end
		local _, loader = setup({ a = def("a"), b = def("b"), c = def("c") })
		local function rows(n)
			return {
				{ id = "a", name = "a", config = { n = n } },
				{ id = "b", name = "b", config = { n = 1 } },
				{ id = "c", name = "c", config = { n = 1 } },
			}
		end
		loader:apply_rows(rows(1)):await()
		local b, c = loader.store.b.innerFiber, loader.store.c.innerFiber
		local buid, cuid = b.uid, c.uid
		loader:apply_rows(rows(2)):await()
		expect.eq(applied.a, 2)
		expect.eq(applied.b, 1)
		expect.eq(applied.c, 1)
		expect.eq(disposed.b, nil)
		expect.eq(disposed.c, nil)
		expect.eq(loader.store.b.innerFiber, b)
		expect.eq(loader.store.c.innerFiber, c)
		expect.eq(b.uid, buid)
		expect.eq(c.uid, cuid)
	end)
	-- the startup profile gets one `plugin <name> (<origin>)` entry per apply, like the old path
	it("records each plugin's apply in the startup profile", function()
		local recorded = {}
		local function def(name, fail)
			return {
				name = name,
				apply = function()
					if fail then
						error("no")
					end
				end,
			}
		end
		-- fresh tables per import, as boot's component() returns
		local _, loader = setup(
			setmetatable({}, {
				__index = function(_, name)
					if name == "a" or name == "b" then
						return def(name)
					end
				end,
			}),
			nil,
			function(label, ms)
				recorded[#recorded + 1] = label
				expect.eq(type(ms), "number")
				expect.eq(ms >= 0, true)
			end
		)
		loader
			:apply_rows({
				{ id = "a", name = "a", config = {}, origin = "user" },
				{ id = "b", name = "b", config = {} },
			})
			:await()
		table.sort(recorded)
		expect.eq(table.concat(recorded, ","), "plugin a (user),plugin b (builtin)")
		expect.eq(byId(loader:dump()).a.state, "ACTIVE")
	end)
	it("extends settled rows additively without reapplying them while waking pending rows", function()
		local applied, cleaned = {}, {}
		local defs = {
			waiter = {
				name = "waiter",
				inject = { "late_svc" },
				apply = function(ctx)
					applied.waiter = (applied.waiter or 0) + 1
					ctx:effect(function()
						return function()
							cleaned.waiter = true
						end
					end)
				end,
			},
			settled = {
				name = "settled",
				apply = function(ctx)
					applied.settled = (applied.settled or 0) + 1
					ctx:effect(function()
						return function()
							cleaned.settled = true
						end
					end)
				end,
			},
			provider = {
				name = "provider",
				provide = { "late_svc" },
				apply = function(ctx)
					applied.provider = (applied.provider or 0) + 1
					ctx:provide("late_svc", { ok = true })
				end,
			},
		}
		local _, loader = setup(defs)
		local rWaiter = { id = "waiter", name = "waiter", config = {}, origin = "builtin" }
		local rSettled = { id = "settled", name = "settled", config = {}, origin = "builtin" }
		loader:apply_rows({ rWaiter, rSettled }):await()
		local settledFiber = loader.store.settled.innerFiber
		expect.eq(applied.settled, 1)
		expect.eq(applied.waiter, nil)
		expect.eq(byId(loader:dump()).waiter.state, "PENDING")

		local rProvider = { id = "provider", name = "provider", config = {}, origin = "user" }
		local updates = 0
		local origUpdate = loader.root.update
		loader.root.update = function(...)
			updates = updates + 1
			return origUpdate(...)
		end
		loader:apply_rows({ rWaiter, rSettled, rProvider }):await()
		expect.eq(updates, 0)
		expect.eq(applied.settled, 1)
		expect.eq(applied.waiter, 1)
		expect.eq(applied.provider, 1)
		expect.eq(loader.store.settled.innerFiber, settledFiber)
		expect.eq(cleaned.settled, nil)
		expect.eq(byId(loader:dump()).waiter.state, "ACTIVE")
		expect.eq(byId(loader:dump()).provider.state, "ACTIVE")
	end)
	it("still reconciles when an existing row table is mutated to disabled, config, or config_error", function()
		local applied, cleaned = 0, false
		local defs = {
			a = {
				name = "a",
				apply = function(ctx)
					applied = applied + 1
					ctx:effect(function()
						return function()
							cleaned = true
						end
					end)
				end,
			},
		}
		local _, loader = setup(defs)
		local updates = 0
		local origUpdate = loader.root.update
		loader.root.update = function(...)
			updates = updates + 1
			return origUpdate(...)
		end
		local row = { id = "a", name = "a", config = { n = 1 } }
		loader:apply_rows({ row }):await()
		expect.eq(applied, 1)
		expect.eq(updates, 1)

		-- replacing row.config in place (simulating internal/commit) bypasses the fast path
		row.config = { n = 2 }
		loader:apply_rows({ row }):await()
		expect.eq(updates, 2)

		-- mutating row.config_error bypasses the fast path as well
		row.config_error = "invalid option"
		loader:apply_rows({ row }):await()
		expect.eq(updates, 3)
		expect.eq(byId(loader:dump()).a.state, "FAILED")
		expect.eq(byId(loader:dump()).a.error, "invalid option")

		row.config_error = nil
		row.disabled = true
		loader:apply_rows({ row }):await()
		expect.eq(updates, 4)
		expect.eq(cleaned, true)
		expect.eq(byId(loader:dump()).a.disabled, true)
	end)

	it("starts N component-wrapped rows without O(N^2) fiber scans or redundant pending wakeups", function()
		local calls = require("mare_kernel.calls")
		local make_component = require("mare_kernel.component")({
			owner = {
				open = function(name)
					return name
				end,
				close = function() end,
				enter = function(_, fn, ...)
					return fn(...)
				end,
			},
		})
		local defs, rows = {}, {}
		local N = 20
		-- 5 providers (p1..p5) + 15 consumers (p6..p20) each injecting all 5 services,
		-- listed consumers-first to exercise static provide -> inject wave ordering.
		for i = N, 6, -1 do
			local name = "p" .. i
			local inject = { "s3_svc_1", "s3_svc_2", "s3_svc_3", "s3_svc_4", "s3_svc_5" }
			defs[name] = make_component.component({
				name = name,
				inject = inject,
				apply = function() end,
			}, "builtin")
			rows[#rows + 1] = { id = name, name = name, config = {}, wave = 1 }
		end
		for i = 5, 1, -1 do
			local name = "p" .. i
			local provide = { "s3_svc_" .. i }
			defs[name] = make_component.component({
				name = name,
				provide = provide,
				apply = function(ctx)
					ctx:provide("s3_svc_" .. i, { id = i })
				end,
			}, "builtin")
			rows[#rows + 1] = { id = name, name = name, config = {}, wave = 0 }
		end

		local root, loader = setup(defs)
		local each_calls, each_visits, pending_wakeups = 0, 0, 0
		local tracked_fibers = {}
		local orig_each = calls.each_fiber
		local orig_notify = cordis.ReflectService.notify
		calls.each_fiber = function(rootCtx, fn)
			each_calls = each_calls + 1
			return orig_each(rootCtx, function(f)
				each_visits = each_visits + 1
				return fn(f)
			end)
		end
		local dispose_plugin = root:on("internal/plugin", function(fiber)
			tracked_fibers[fiber] = true
		end)
		cordis.ReflectService.notify = function(self, names, ...)
			for fiber in pairs(tracked_fibers) do
				if fiber.state == cordis.FiberState.PENDING and fiber.inject then
					for _, dep in ipairs(names) do
						if fiber.inject[dep] then
							pending_wakeups = pending_wakeups + 1
							break
						end
					end
				end
			end
			return orig_notify(self, names, ...)
		end
		local ok, err = pcall(function()
			loader:apply_rows(rows):await()
		end)
		calls.each_fiber = orig_each
		cordis.ReflectService.notify = orig_notify
		dispose_plugin()
		assert(ok, err)

		local d = byId(loader:dump())
		for i = 1, N do
			expect.eq(d["p" .. i].state, "ACTIVE")
		end
		expect.eq(each_calls <= 2, true, "each_fiber calls must be <= 2, got " .. tostring(each_calls))
		expect.eq(each_visits <= 4 * N, true, "each_fiber visits must be <= 4*N, got " .. tostring(each_visits))
		expect.eq(
			pending_wakeups,
			0,
			"wave-ordered start must not trigger PENDING consumer wakeups, got " .. tostring(pending_wakeups)
		)
	end)

	it(
		"runs multi-wave startup without blocking wave 1 on slow or failing wave 0 rows and preserves cross-wave PENDING",
		function()
			local applied = {}
			local slow_done = false
			local unrelated_ran_before_slow = false
			local fast_consumer_ran_before_slow = false
			local saw_all_rows_during_w0 = false
			local loader
			local defs = {
				consumer = {
					name = "consumer",
					inject = { "dyn_svc" },
					apply = function()
						applied[#applied + 1] = "consumer"
					end,
				},
				orphan = {
					name = "orphan",
					inject = { "never_provided_svc" },
					apply = function()
						applied[#applied + 1] = "orphan"
					end,
				},
				failing_w0 = {
					name = "failing_w0",
					apply = function()
						error("wave 0 failure")
					end,
				},
				slow_w0 = {
					name = "slow_w0",
					apply = cordis.async(function()
						if loader and #loader.root.data == 8 then
							saw_all_rows_during_w0 = true
						end
						mare.task.sleep(120)
						slow_done = true
						applied[#applied + 1] = "slow_w0"
					end),
				},
				fast_provide_w0 = {
					name = "fast_provide_w0",
					provide = { "fast_svc" },
					apply = function(ctx)
						ctx:provide("fast_svc", { ok = true })
					end,
				},
				dyn_provider = {
					name = "dyn_provider",
					-- wave 1 dynamic provider unblocking wave 0 consumer via Cordis PENDING
					apply = function(ctx)
						applied[#applied + 1] = "dyn_provider"
						ctx:provide("dyn_svc", { ok = true })
					end,
				},
				unrelated_w1 = {
					name = "unrelated_w1",
					apply = function()
						unrelated_ran_before_slow = not slow_done
						applied[#applied + 1] = "unrelated_w1"
					end,
				},
				fast_consumer = {
					name = "fast_consumer",
					inject = { "fast_svc" },
					apply = function()
						fast_consumer_ran_before_slow = not slow_done
						applied[#applied + 1] = "fast_consumer"
					end,
				},
			}
			_, loader = setup(defs)
			loader
				:apply_rows({
					{ id = "consumer", name = "consumer", config = {}, wave = 0 },
					{ id = "orphan", name = "orphan", config = {}, wave = 0 },
					{ id = "failing_w0", name = "failing_w0", config = {}, wave = 0 },
					{ id = "slow_w0", name = "slow_w0", config = {}, wave = 0 },
					{ id = "fast_provide_w0", name = "fast_provide_w0", config = {}, wave = 0 },
					{ id = "dyn_provider", name = "dyn_provider", config = {}, wave = 1 },
					{ id = "unrelated_w1", name = "unrelated_w1", config = {}, wave = 1 },
					{ id = "fast_consumer", name = "fast_consumer", config = {}, wave = 1 },
				})
				:await()
			local d = byId(loader:dump())
			expect.eq(d.dyn_provider.state, "ACTIVE")
			expect.eq(d.consumer.state, "ACTIVE")
			expect.eq(d.failing_w0.state, "FAILED")
			expect.eq(d.slow_w0.state, "ACTIVE")
			expect.eq(d.fast_provide_w0.state, "ACTIVE")
			expect.eq(d.unrelated_w1.state, "ACTIVE")
			expect.eq(d.fast_consumer.state, "ACTIVE")
			expect.eq(d.orphan.state, "PENDING")
			expect.eq(d.orphan.missing[1], "never_provided_svc")
			expect.eq(
				unrelated_ran_before_slow,
				true,
				"wave 1 unrelated row must not be blocked by slow async wave 0 row"
			)
			expect.eq(
				fast_consumer_ran_before_slow,
				true,
				"wave 1 consumer of fast wave 0 provider must not be blocked by slow wave 0 row"
			)
			expect.eq(saw_all_rows_during_w0, true, "root.data must contain all rows during wave 0")
		end
	)

	it("applies best-effort wave startup to additively added user/package rows after builtins settle", function()
		local make_component = require("mare_kernel.component")({
			owner = {
				open = function(name)
					return name
				end,
				close = function() end,
				enter = function(_, fn, ...)
					return fn(...)
				end,
			},
		})
		local builtin_applies = 0
		local slow_done = false
		local unrelated_ran_before_slow = false
		local defs = {
			core_builtin = make_component.component({
				name = "core_builtin",
				provide = { "s3b_builtin_svc" },
				apply = function(ctx)
					builtin_applies = builtin_applies + 1
					ctx:provide("s3b_builtin_svc", { ok = true })
				end,
			}, "builtin"),
		}
		local rBuiltin = { id = "core_builtin", name = "core_builtin", config = {}, origin = "builtin", wave = 0 }
		local added_rows = {}
		local N = 20
		-- 15 wave-2 consumers each injecting 5 wave-1 user services, listed consumers-first
		for i = N, 6, -1 do
			local name = "u" .. i
			local inject = { "s3b_u_1", "s3b_u_2", "s3b_u_3", "s3b_u_4", "s3b_u_5" }
			defs[name] = make_component.component({
				name = name,
				inject = inject,
				apply = function() end,
			}, "user")
			added_rows[#added_rows + 1] = { id = name, name = name, config = {}, origin = "user", wave = 2 }
		end
		for i = 5, 1, -1 do
			local name = "u" .. i
			local provide = { "s3b_u_" .. i }
			defs[name] = make_component.component({
				name = name,
				inject = { "s3b_builtin_svc" },
				provide = provide,
				apply = function(ctx)
					ctx:provide("s3b_u_" .. i, { id = i })
				end,
			}, "user")
			added_rows[#added_rows + 1] = { id = name, name = name, config = {}, origin = "user", wave = 1 }
		end
		defs.u_slow_w1 = make_component.component({
			name = "u_slow_w1",
			apply = cordis.async(function()
				mare.task.sleep(120)
				slow_done = true
			end),
		}, "user")
		defs.u_unrelated_w2 = make_component.component({
			name = "u_unrelated_w2",
			apply = function()
				unrelated_ran_before_slow = not slow_done
			end,
		}, "user")
		defs.u_cross_consumer = make_component.component({
			name = "u_cross_consumer",
			inject = { "s3b_u_dyn" },
			apply = function() end,
		}, "user")
		defs.u_cross_provider = make_component.component({
			name = "u_cross_provider",
			apply = function(ctx)
				ctx:provide("s3b_u_dyn", { ok = true })
			end,
		}, "user")
		added_rows[#added_rows + 1] = { id = "u_slow_w1", name = "u_slow_w1", config = {}, origin = "user", wave = 1 }
		added_rows[#added_rows + 1] =
			{ id = "u_unrelated_w2", name = "u_unrelated_w2", config = {}, origin = "user", wave = 2 }
		added_rows[#added_rows + 1] =
			{ id = "u_cross_consumer", name = "u_cross_consumer", config = {}, origin = "user", wave = 1 }
		added_rows[#added_rows + 1] =
			{ id = "u_cross_provider", name = "u_cross_provider", config = {}, origin = "user", wave = 2 }

		local root, loader = setup(defs)
		loader:apply_rows({ rBuiltin }):await()
		local builtinFiber = loader.store.core_builtin.innerFiber
		expect.eq(builtin_applies, 1)

		local all_rows = { rBuiltin }
		for _, r in ipairs(added_rows) do
			all_rows[#all_rows + 1] = r
		end
		local updates, pending_wakeups = 0, 0
		local tracked_fibers = {}
		local origUpdate = loader.root.update
		local origNotify = cordis.ReflectService.notify
		loader.root.update = function(...)
			updates = updates + 1
			return origUpdate(...)
		end
		local dispose_plugin = root:on("internal/plugin", function(fiber)
			tracked_fibers[fiber] = true
		end)
		cordis.ReflectService.notify = function(self, names, ...)
			for fiber in pairs(tracked_fibers) do
				if fiber.state == cordis.FiberState.PENDING and fiber.inject then
					for _, dep in ipairs(names) do
						if fiber.inject[dep] then
							pending_wakeups = pending_wakeups + 1
							break
						end
					end
				end
			end
			return origNotify(self, names, ...)
		end
		local ok, err = pcall(function()
			loader:apply_rows(all_rows):await()
		end)
		loader.root.update = origUpdate
		cordis.ReflectService.notify = origNotify
		dispose_plugin()
		assert(ok, err)

		expect.eq(updates, 0, "additive wave start must bypass full root:update")
		expect.eq(builtin_applies, 1)
		expect.eq(loader.store.core_builtin.innerFiber, builtinFiber)
		expect.eq(unrelated_ran_before_slow, true, "additive wave 2 must not wait on slow wave 1 row")
		local d = byId(loader:dump())
		for i = 1, N do
			expect.eq(d["u" .. i].state, "ACTIVE")
		end
		expect.eq(d.u_slow_w1.state, "ACTIVE")
		expect.eq(d.u_unrelated_w2.state, "ACTIVE")
		expect.eq(d.u_cross_consumer.state, "ACTIVE")
		expect.eq(d.u_cross_provider.state, "ACTIVE")
		-- Only the 1 explicit cross-wave consumer waits in PENDING until its wave-2 provider runs.
		expect.eq(
			pending_wakeups,
			1,
			"expected only the 1 cross-wave PENDING consumer wakeup, got " .. tostring(pending_wakeups)
		)
	end)

	it("dump({ effects = false }) skips getEffects while matching all other fields", function()
		local defs = {
			provider = {
				name = "provider",
				apply = function(ctx)
					ctx:effect(function()
						return function() end
					end, "work")
					ctx:provide("svc_dump_opt", { ok = true })
				end,
			},
			consumer = {
				name = "consumer",
				inject = { "svc_dump_opt" },
				apply = function() end,
			},
		}
		local _, loader = setup(defs)
		loader
			:apply_rows({
				{ id = "provider", name = "provider", config = {}, origin = "builtin" },
				{ id = "consumer", name = "consumer", config = {}, origin = "user" },
			})
			:await()

		local full = byId(loader:dump())
		local fast = byId(loader:dump({ effects = false }))
		expect.eq(full.provider.effects >= 1, true)
		expect.eq(fast.provider.effects, nil)
		expect.eq(fast.consumer.effects, nil)
		for _, id in ipairs({ "provider", "consumer" }) do
			expect.eq(fast[id].state, full[id].state)
			expect.eq(fast[id].origin, full[id].origin)
			expect.eq(fast[id].disabled, full[id].disabled)
			expect.eq(#fast[id].provides, #full[id].provides)
			for i = 1, #full[id].provides do
				expect.eq(fast[id].provides[i], full[id].provides[i])
			end
		end
	end)

	it("component merges owner and active teardown in LIFO order and skips inactive cleanup on error", function()
		local events = {}
		local next_id = 0
		local prims = {
			owner = {
				open = function(name)
					next_id = next_id + 1
					events[#events + 1] = "open:" .. name
					return next_id
				end,
				close = function(id)
					events[#events + 1] = "close:" .. tostring(id)
				end,
				enter = function(_, fn, ...)
					return fn(...)
				end,
			},
			mark_active = function(name, owner)
				events[#events + 1] = (owner and "active:" or "inactive:") .. name
			end,
			unload_modules = function(name)
				events[#events + 1] = "unload:" .. name
			end,
		}
		local comp_factory = require("mare_kernel.component")(prims)
		local defs = {
			good = comp_factory.component({
				name = "good",
				apply = function() end,
			}, "user"),
			bad = comp_factory.component({
				name = "bad",
				apply = function()
					error("boom")
				end,
			}, "user"),
		}
		local _, loader = setup(defs)
		loader
			:apply_rows({
				{ id = "good", name = "good", config = {}, origin = "user" },
				{ id = "bad", name = "bad", config = {}, origin = "user" },
			})
			:await()
		local d = byId(loader:dump())
		expect.eq(d.good.state, "ACTIVE")
		expect.eq(d.bad.state, "FAILED")
		-- Merged lifecycle registers a single effect for owner+active.
		expect.eq(d.good.effects, 1)

		loader:remove_rows({ "good" }):await()
		-- Teardown of `good` must mark inactive BEFORE closing owner and unloading modules.
		local idx_inactive, idx_close, idx_unload
		for i, ev in ipairs(events) do
			if ev == "inactive:good" then
				idx_inactive = i
			elseif ev == "close:1" then
				idx_close = i
			elseif ev == "unload:good" then
				idx_unload = i
			end
			expect.eq(ev == "inactive:bad", false, "failing apply must never call mark_active(nil)")
		end
		expect.eq(idx_inactive ~= nil and idx_close ~= nil and idx_unload ~= nil, true)
		expect.eq(idx_inactive < idx_close and idx_close < idx_unload, true)
	end)

	it("dump dedups duplicate provision names on a childless row", function()
		local defs = {
			dup = {
				name = "dup",
				apply = function(ctx)
					ctx:provide("svc_dup", { v = 1 })
					local iso = ctx:isolate("svc_dup")
					iso:provide("svc_dup", { v = 2 })
				end,
			},
		}
		local _, loader = setup(defs)
		loader:apply_rows({ { id = "dup", name = "dup", config = {}, origin = "user" } }):await()
		local full = byId(loader:dump())
		local fast = byId(loader:dump({ effects = false }))
		expect.eq(#full.dup.provides, 1)
		expect.eq(full.dup.provides[1], "svc_dup")
		expect.eq(#fast.dup.provides, 1)
		expect.eq(fast.dup.provides[1], "svc_dup")
	end)
end)
