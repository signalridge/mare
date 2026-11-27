-- The bus host plugin (crates/mare-kernel/lua/mare_kernel/plugins/bus.lua):
-- an emit / waterfall to nobody returns at once but keeps the Rust entry's emitter check, every
-- other call goes through the Rust entry, and the service shows plugins nothing but its five emit
-- methods (no listener query).
local describe = mare.test.describe
local it = mare.test.it
local expect = mare.test.expect

local booted = require("mare_kernel.test").boot()
local cordis, host = booted.cordis, booted.host

-- stub prims and bus: `quiet` events answer true; `closed` makes the emitter check raise as the
-- Rust entry does for a closed owner; every dispatch is logged
local function setup(quiet)
	local log = { dispatched = {}, checks = 0 }
	local state = { closed = false }
	local prims = {
		events = {
			dispatch = function(mode, event, ...)
				log.dispatched[#log.dispatched + 1] = mode .. " " .. event
				if mode == "waterfall" or mode == "waterfall_checked" then
					return (...)
				end
			end,
			emitter_live = function()
				log.checks = log.checks + 1
				if state.closed then
					error("plugin owner 7 is closed", 0)
				end
			end,
		},
	}
	local bus = {
		quiet = function(event)
			return quiet[event] == true
		end,
	}
	local root = cordis.Context.new({ host = host })
	cordis.await(root:use(require("mare_kernel.plugins.bus"), { prims = prims, bus = bus }))
	local got
	cordis.await(root:use({
		name = "consumer",
		inject = { "bus" },
		apply = function(ctx)
			got = ctx.bus
		end,
	}))
	cordis.tick(host)
	return got, log, state
end

describe("bus host plugin", function()
	it("returns at once for an event nobody takes, keeping the emitter check", function()
		local bus, log = setup({ ["probe/q"] = true })
		expect.eq(bus:emit("probe/q", { n = 1 }), nil)
		expect.eq(bus:waterfall("probe/q", "v", { k = 1 }), "v")
		expect.eq(bus:waterfall_checked("probe/q", "w"), "w")
		expect.eq(#log.dispatched, 0)
		expect.eq(log.checks, 3)
	end)

	it("refuses a quiet emit from a closed owner, as the Rust entry does", function()
		local bus, log, state = setup({ ["probe/q"] = true })
		state.closed = true
		local ok, err = pcall(bus.emit, bus, "probe/q", {})
		expect.eq(ok, false)
		expect.eq(tostring(err):find("plugin owner 7 is closed", 1, true) ~= nil, true)
		expect.eq(#log.dispatched, 0)
	end)

	it("dispatches through the Rust entry when someone takes the event, and always for bail / emit_checked", function()
		local bus, log = setup({ ["probe/q"] = true })
		bus:emit("probe/heard", {})
		expect.eq(bus:waterfall("probe/heard", "v"), "v")
		bus:bail("probe/q", {})
		bus:emit_checked("probe/q", {})
		expect.eq(
			table.concat(log.dispatched, ","),
			"emit probe/heard,waterfall probe/heard,bail probe/q,emit_checked probe/q"
		)
		expect.eq(log.checks, 0)
	end)

	it("shows plugins only its five emit methods", function()
		local bus = setup({})
		expect.eq(bus.quiet, nil)
		expect.eq(bus.dispatch, nil)
		expect.eq(bus.emitter_live, nil)
		for _, mode in ipairs({ "emit", "emit_checked", "bail", "waterfall", "waterfall_checked" }) do
			expect.eq(bus[mode] ~= nil, true)
		end
	end)
end)
