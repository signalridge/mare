local t = require("mare.test")
local describe = t.describe
local it = t.it
local before_each = t.before_each
local after_each = t.after_each
local expect = t.expect

describe("mare.test", function()
	it("exposes the same module on mare.test and require('mare.test')", function()
		expect.eq(t, mare.test)
	end)

	it("checks deep table equality and inequality", function()
		expect.eq({ a = 1, b = { 2, 3 } }, { a = 1, b = { 2, 3 } })
		expect.ne({ a = 1, b = { 2, 3 } }, { a = 1, b = { 2, 4 } })
		expect.ne({ a = 1 }, { a = 1, b = 2 })
	end)

	it("ignores __eq, __pairs, and __index metamethods in deep_eq", function()
		local mt = {
			__eq = function()
				return true
			end,
			__pairs = function()
				return function()
					return nil
				end
			end,
			__index = { hidden = 99 },
		}
		local a = setmetatable({ value = 1 }, mt)
		local b = setmetatable({ value = 2 }, mt)
		expect.ne(a, b)

		local c = setmetatable({ value = 1 }, {
			__index = { extra = 42 },
		})
		expect.eq(c, { value = 1 })
		expect.ne({ value = 1, extra = 42 }, c)
		expect.ne(c, { value = 1, extra = 42 })
	end)

	it("distinguishes mare.json.array({}) from {} in deep_eq", function()
		expect.ne(mare.json.array({}), {})
		expect.eq(mare.json.array({}), mare.json.array({}))
		expect.eq(mare.json.array({ 1, 2 }), mare.json.array({ 1, 2 }))
		expect.eq(mare.json.array({ 1, 2 }), { 1, 2 })
	end)

	it("checks truthy, falsy, match, and error", function()
		expect.truthy(true)
		expect.truthy(1)
		expect.truthy("")
		expect.falsy(false)
		expect.falsy(nil)
		expect.match("hello mare 123", "mare %d+")
		expect.error(function()
			error("boom: 42")
		end, "boom: %d+")
	end)

	it("supports deterministic expect.snapshot", function()
		expect.snapshot({
			z = "last",
			a = 1,
			nested = { items = mare.json.array({ "x", "y" }) },
		}, "basic_table")
	end)

	describe("hooks ordering", function()
		local log = {}

		before_each(function()
			log[#log + 1] = "outer_before"
		end)

		after_each(function()
			log[#log + 1] = "outer_after"
		end)

		describe("inner", function()
			before_each(function()
				log[#log + 1] = "inner_before"
			end)

			after_each(function()
				log[#log + 1] = "inner_after"
			end)

			it("runs before_each outer-to-inner", function()
				log[#log + 1] = "test_1"
				expect.eq(log, { "outer_before", "inner_before", "test_1" })
			end)

			it("ran after_each inner-to-outer after the previous test", function()
				log[#log + 1] = "test_2"
				expect.eq(log, {
					"outer_before",
					"inner_before",
					"test_1",
					"inner_after",
					"outer_after",
					"outer_before",
					"inner_before",
					"test_2",
				})
			end)
		end)
	end)

	describe("mare.test.host", function()
		local counter_contract = {
			methods = {
				add = {
					doc = "Add n to the counter and return the new total.",
					params = {
						type = "object",
						properties = { n = { type = "integer" } },
						required = { "n" },
						additionalProperties = false,
					},
					order = { "n" },
					result = { type = "integer" },
				},
			},
		}

		it("gates inject, runs contract admission and guards, and withdraws on dispose", function()
			local host = t.host()
			local cleaned = false
			local total = 0

			host:load({
				name = "counter_consumer",
				inject = { "counter_demo" },
				apply = function(ctx)
					---@type { add: fun(self: table, n: integer): integer }
					local counter = assert(ctx:get("counter_demo"))
					total = counter:add(4)
					ctx:effect(function()
						return function()
							cleaned = true
						end
					end)
				end,
			})
			expect.eq(host:state("counter_consumer"), "PENDING")
			expect.error(function()
				expect.settled(host)
			end, "PENDING")

			-- Contract admission rejects missing required methods.
			expect.error(function()
				host:provide("counter_demo", {}, { contract = counter_contract })
			end)

			local value = 10
			local withdraw = host:provide("counter_demo", {
				add = function(_, n)
					value = value + n
					return value
				end,
			}, { contract = counter_contract })

			expect.eq(host:state("counter_consumer"), "ACTIVE")
			expect.settled(host)
			expect.eq(total, 14)

			-- Runtime contract guards validate arguments and results.
			local svc = host:get("counter_demo")
			expect.eq(svc:add(6), 20)
			expect.error(function()
				svc:add("not_an_integer")
			end, "invalid_arguments")

			-- Withdrawing the provider cleans up the dependent and moves it back to PENDING.
			withdraw()
			expect.eq(cleaned, true)
			expect.eq(host:get("counter_demo"), nil)
			expect.eq(host:state("counter_consumer"), "PENDING")

			host:dispose()
			expect.eq(host:state("counter_consumer"), "DISPOSED")
		end)

		it("rolls back to the previous version when reload fails", function()
			local host = t.host()
			local version = 0
			local disposed = {}

			host:load({
				name = "reload_demo",
				apply = function(ctx)
					version = 1
					ctx:effect(function()
						return function()
							disposed[#disposed + 1] = "v1"
						end
					end)
				end,
			})
			expect.settled(host)
			expect.eq(version, 1)

			local ok, err = host:reload("reload_demo", {
				name = "reload_demo",
				apply = function()
					error("broken v2")
				end,
			})
			expect.falsy(ok)
			expect.match(assert(err), "broken v2")
			expect.eq(host:state("reload_demo"), "ACTIVE")
			expect.eq(version, 1)

			local ok2 = host:reload("reload_demo", {
				name = "reload_demo",
				apply = function()
					version = 2
				end,
			})
			expect.truthy(ok2)
			expect.settled(host)
			expect.eq(version, 2)
			expect.eq(disposed[#disposed], "v1")
			host:dispose()
		end)

		it("isolates multiple test hosts and withdraws tool registrations on dispose", function()
			local a = t.host({ builtins = { "tools" } })
			local b = t.host({ builtins = { "tools" } })
			expect.settled(a)
			expect.settled(b)

			a:load({
				name = "only_in_a",
				inject = { "tools" },
				apply = function(ctx)
					ctx.tools:register("probe_a", {
						description = "Probe tool in host A",
						parameters = { type = "object", properties = {} },
						execute = function()
							return { content = "ok" }
						end,
					})
				end,
			})
			expect.settled(a)
			expect.truthy(a:get("tools"):get("probe_a"))
			expect.eq(b:get("tools"):get("probe_a"), nil)

			a:dispose()
			expect.eq(a:get("tools"), nil)
			expect.truthy(b:get("tools"))
			b:dispose()
		end)
	end)
end)
