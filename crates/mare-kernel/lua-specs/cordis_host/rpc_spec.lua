-- Host plugins `rpc` and `hooks` (crates/mare-kernel/lua/mare_kernel/plugins/{rpc,hooks}.lua;
-- API6 B16): connection lifecycle lives on `rpc`, and remote hooks subscribe through `hooks`
-- using `callback.call` semantics (emit / parallel / serial / waterfall).
local describe = mare.test.describe
local it = mare.test.it
local expect = mare.test.expect

local booted = require("mare_kernel.test").boot()
local cordis, host = booted.cordis, booted.host
local makeBus = require("mare_kernel.bus")
local Rpc = require("mare_kernel.plugins.rpc")
local Hooks = require("mare_kernel.plugins.hooks")

local function stub(answer)
	local sent = {}
	local prims = {
		rpc = {},
		policy = { points = {}, rewrite_points = {} },
		owner = {
			open = function()
				return 1
			end,
			close = function() end,
			enter = function(_, fn, ...)
				return fn(...)
			end,
		},
	}
	local modes = {
		["queue/changed"] = "emit",
		["request/prepare"] = "serial",
		["probe/collect"] = "parallel",
		["request/build"] = "waterfall",
	}
	function prims.rpc.remote_mode(event)
		return modes[event]
	end
	function prims.rpc.revoked()
		return nil
	end
	function prims.rpc.callbacks_active()
		return true
	end
	function prims.rpc.may_subscribe()
		return nil
	end
	function prims.rpc.project(_, ...)
		local args = table.pack(...)
		args.n = nil
		return args, args[1]
	end
	function prims.rpc.restore(_, v)
		return v
	end
	function prims.rpc.with_timeout(_, fn, ...)
		return fn(...)
	end
	local function make_handle()
		return function(event, mode, args)
			local params = { event = event, mode = mode, args = args }
			sent[#sent + 1] = { method = "callback.call", params = params }
			return answer(params)
		end
	end
	return prims, sent, make_handle
end

local function setup(answer)
	local prims, sent, make_handle = stub(answer)
	local root = cordis.Context.new({ host = host })
	cordis.await(root:use(Rpc, { prims = prims }))
	cordis.await(root:use(Hooks(prims, cordis.Service)))
	local rpc = root:get("rpc")
	cordis.await(rpc:connect(1))
	local conn_ctx = rpc:context(1)
	local hooks = conn_ctx:get("hooks")
	return root, rpc, hooks, sent, make_handle
end

describe("rpc and hooks host plugins", function()
	it("refuses events the protocol does not allow remotely", function()
		local _, _, hooks, _, make_handle = setup(function() end)
		local ok, err = pcall(hooks.subscribe, hooks, "tool/before", { handle = make_handle() })
		expect.eq(ok, false)
		expect.eq(err.code, "invalid_arguments")
	end)

	it("delivers emit, serial, and parallel through callback.call handle", function()
		local root, _, hooks, sent, make_handle = setup(function(params)
			if params.mode == "emit" then
				return nil
			end
			return { value = "from-remote:" .. params.event }
		end)
		hooks:subscribe("queue/changed", { handle = make_handle() })
		hooks:subscribe("request/prepare", { handle = make_handle() })
		hooks:subscribe("probe/collect", { handle = make_handle() })
		root:emit("queue/changed", { items = {}, size = 2 })
		expect.eq(sent[1].method, "callback.call")
		expect.eq(sent[1].params.mode, "emit")
		expect.eq(sent[1].params.args[1].size, 2)
		local first = cordis.await(root:serial("request/prepare", "x"))
		expect.eq(first, "from-remote:request/prepare")
		cordis.await(root:parallel("probe/collect"))
		expect.eq(sent[#sent].method, "callback.call")
		expect.eq(sent[#sent].params.mode, "parallel")
	end)

	it("lets a remote waterfall step pass on, short-circuit, or rewrite the value", function()
		local action
		local root, _, hooks, _, make_handle = setup(function(params)
			if action == "pass" then
				return { next = true }
			elseif action == "short" then
				return { value = "short" }
			end
			return { next = true, value = tostring(params.args[1]) .. "+remote" }
		end)
		local bus = makeBus(root, {
			events = {
				tapped = function()
					return false
				end,
			},
		})
		hooks:subscribe("request/build", { handle = make_handle() })
		root:on("request/build", function(v, next)
			return next(tostring(v) .. "+inner")
		end)
		local function run()
			return bus.waterfall("request/build", "base")
		end
		action = "pass"
		expect.eq(run(), "base+inner")
		action = "short"
		expect.eq(run(), "short")
		action = "rewrite"
		expect.eq(run(), "base+remote+inner")
	end)

	it("drops the connection's hooks when it closes", function()
		local root, rpc, hooks, sent, make_handle = setup(function()
			return nil
		end)
		hooks:subscribe("queue/changed", { handle = make_handle() })
		cordis.await(rpc:disconnect(1))
		root:emit("queue/changed", { items = {} })
		expect.eq(#sent, 0)
		local ok, err = pcall(rpc.provide, rpc, 1, "x", {})
		expect.eq(ok, false)
		expect.eq(err.code, "inactive_ctx")
	end)

	it("forgets a connection that closes before its fiber applied", function()
		local _, rpc = setup(function() end)
		rpc:connect(2)
		expect.eq(rpc:context(2) ~= nil, true)
		cordis.await(rpc:disconnect(2))
		cordis.tick(host)
		expect.eq(rpc:context(2), nil)
	end)
end)
