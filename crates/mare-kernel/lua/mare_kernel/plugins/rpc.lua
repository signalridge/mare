-- Host plugin `rpc`:
-- every RPC connection is a fiber; closing the connection disposes the fiber, and with it every
-- owned registration or remote service. Remote hooks now subscribe through `hooks.subscribe`
-- (`mare_kernel.plugins.hooks`) using `callback.call`.
--
--   rpc:connect(id) the connection's fiber (`id` = the frontend's connection id)
--   rpc:disconnect(id) dispose it
--   rpc:context(id) the connection's cordis context (nil once closed)
--   rpc:provide(id, name, impl, options) provide a service on the connection's fiber
local cordis = require("cordis")
local calls = require("mare_kernel.calls")

local RpcService = cordis.class("RpcService", cordis.Service)

function RpcService:constructor(ctx, config)
	cordis.Service.constructor(self, ctx, "rpc")
	self.prims = config.prims
	self.fibers = {}
	self.contexts = {}
	self.ids = setmetatable({}, { __mode = "k" }) -- [connection fiber] = connection id
end

local function fail(code, message)
	error(calls.coded(code, message), 0)
end

function RpcService:connect(id)
	if self.fibers[id] then
		return self.fibers[id]
	end
	local this = self
	local fiber = self.ctx:plugin({ name = "rpc:" .. tostring(id), apply = function() end })
	fiber.ctx:effect(function()
		return function()
			this.contexts[id] = nil
		end
	end, "rpc connection")
	self.contexts[id] = fiber.ctx
	self.fibers[id] = fiber
	self.ids[fiber] = id
	return fiber
end

---The connection's cordis context (nil once closed): services called with it see the connection as
---the caller, and what they register goes with the connection's fiber.
function RpcService:context(id)
	return self.contexts[id]
end

function RpcService:disconnect(id)
	local fiber = self.fibers[id]
	self.fibers[id] = nil
	if fiber then
		return fiber:dispose()
	end
end

---`services.register` on the tree: provide `impl` as service `name` from the connection's fiber, admitted
---under its contract like any provide (contracts host plugin), running as the connection's owner
---(`id` is the connection's owner id). Returns the disposer; the provision also goes with the connection.
function RpcService:provide(id, name, impl, options)
	local ctx = self.contexts[id]
	if not ctx then
		fail("inactive_ctx", "the connection is closed")
	end
	local prims, ids = self.prims, self.ids
	local function caller(svc)
		local _, owner = calls.resolve_caller(svc.ctx and svc.ctx.fiber, ids)
		return owner
	end
	local Remote = cordis.class("rpc " .. name, mare.Service)
	function Remote:constructor(c)
		for method, f in pairs(impl) do
			self[method] = function(svc, ...)
				return f(svc, caller(svc), ...)
			end
		end
		Remote.super.constructor(self, c, name, options)
	end
	return calls.as_owner(prims, id, function()
		local fiber = ctx:plugin(Remote)
		local ok, raised = pcall(cordis.await, fiber)
		if not ok or fiber.state ~= cordis.FiberState.ACTIVE then
			local err = (not ok and raised) or fiber.error
			local done = fiber:dispose()
			if done then
				done:await()
			end
			error(err or ("service `" .. name .. "` did not start"), 0)
		end
		return function()
			local done = fiber:dispose()
			if done then
				done:await()
			end
		end
	end)
end

return RpcService
