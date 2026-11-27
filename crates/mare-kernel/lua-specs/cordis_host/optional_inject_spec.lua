-- An optional service that comes up after its consumer (on the cordis tree loading follows
-- service availability): the consumer's `ctx:inject({ name }, fn)` runs when the service appears
-- and registers into it; what it registered goes when the service goes away. The consumer itself
-- does not restart. Built-ins use this for registrations into optional services at apply time.
local describe = mare.test.describe
local it = mare.test.it
local expect = mare.test.expect

local booted = require("mare_kernel.test").boot()
local cordis, host = booted.cordis, booted.host

describe("optional service through inject", function()
	it("runs the follower when the service comes up later and undoes it when the service goes", function()
		local root = cordis.Context.new({ host = host })
		local applied, registered = 0, {}
		-- a native component; legacy's `ctx:inject({ names }, fn)` forwards to the same cordis call
		local consumer = {
			name = "oi_consumer",
			apply = function(ctx)
				applied = applied + 1
				-- apply-time read: the service is not up yet
				registered.at_apply = ctx:get("oi_late") ~= nil
				ctx:inject({ "oi_late" }, function(ctx)
					ctx.oi_late:add("from-consumer")
				end)
			end,
		}
		cordis.await(root:use(consumer))
		expect.eq(registered.at_apply, false)

		local items = {}
		-- a service: inside its method `self.ctx` is the caller (the consumer's child fiber), so the
		-- registration is an effect of that fiber
		local Late = cordis.class("Late", cordis.Service)
		function Late:constructor(ctx)
			cordis.Service.constructor(self, ctx, "oi_late")
		end
		function Late:add(item)
			items[#items + 1] = item
			self.ctx:effect(function()
				return function()
					for i, v in ipairs(items) do
						if v == item then
							table.remove(items, i)
							break
						end
					end
				end
			end)
		end
		local provider = root:use(Late)
		cordis.await(provider)
		cordis.tick(host)
		mare.task.sleep(10)
		expect.eq(table.concat(items, ","), "from-consumer")
		expect.eq(applied, 1) -- the consumer itself did not restart

		cordis.await(provider:dispose())
		mare.task.sleep(10)
		expect.eq(#items, 0)
		expect.eq(applied, 1)
	end)
end)
