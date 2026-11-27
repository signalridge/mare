-- Host plugin `kernel`: the old Ctx introspection and reload methods as one contracted service,
-- `kernel`, backed by `types/mare/services/contracts/kernel.lua` (`provider = "host"`).
--
-- `require(...)(prims, Service)`: reads `prims.kernel`, `prims.reload`, `prims.contracts`, and
-- `prims.owner`; Service is boot's mare.Service.
local cordis = require("cordis")
local calls = require("mare_kernel.calls")

return function(prims, Service)
	local Kernel = cordis.class("KernelService", Service)
	local k = prims.kernel
	local reg = prims.contracts.register_host_method
	local reg_async = prims.contracts.register_host_method_async

	reg("kernel", "plugins", function()
		return k.plugins()
	end)
	reg("kernel", "problems", function()
		return k.problems()
	end)
	reg("kernel", "stalled", function()
		return k.stalled()
	end)
	reg("kernel", "plugin_row", function(name)
		return k.plugin_row(name)
	end)
	reg("kernel", "is_builtin", function(name)
		return k.origin(name) == "builtin"
	end)
	-- `calls.active_ctx()` is the caller's context (mare.Service).
	reg("kernel", "provides", function(name)
		local caller_ctx = calls.active_ctx()
		local impl = caller_ctx and caller_ctx.reflect:getImpl(name, false) or nil
		return impl ~= nil and impl.ctx.fiber == caller_ctx.fiber
	end)
	reg_async("kernel", "reload", function(name)
		return k.reload(name)
	end)
	reg_async("kernel", "reload_barrier", function()
		return prims.reload.barrier()
	end)
	reg("kernel", "reload_generation", function()
		return prims.reload.generation()
	end)
	reg("kernel", "session_epoch", function()
		return k.session_epoch()
	end)

	-- admission needs a running owner: the host plugin's own record, closed with its fiber
	function Kernel:constructor(ctx)
		if not prims.contracts.declared("kernel") then
			return
		end
		local owner = prims.owner.open("kernel", "root")
		ctx:effect(function()
			return function()
				prims.owner.close(owner)
			end
		end, "owner")
		prims.owner.enter(owner, Kernel.super.constructor, self, ctx, "kernel")
	end

	return Kernel
end
