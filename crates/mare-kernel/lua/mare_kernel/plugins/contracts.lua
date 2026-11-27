-- Host plugin `contracts`: `mare.contracts`, the public reads of
-- the service contracts (were ctx:permitted / ctx:validate). A service is admitted under its contract
-- by mare.Service (mare_kernel/service.lua); nothing provides a plain table any more.
local cordis = require("cordis")
local calls = require("mare_kernel.calls")

local ContractsService = cordis.class("ContractsService", cordis.Service)

function ContractsService:constructor(ctx, config)
	cordis.Service.constructor(self, ctx, "contracts")
	local contracts = config.prims.contracts
	local rpc_svc
	local function resolve_caller_id(caller)
		if type(caller) == "number" then
			return caller
		end
		if type(caller) == "table" and caller.fiber ~= nil then
			rpc_svc = rpc_svc or ctx:get("rpc")
			return calls.resolve_caller(caller.fiber, rpc_svc and rpc_svc.ids)
		end
		return nil
	end
	mare.contracts = {
		permitted = function(service, method, caller)
			return contracts.permitted(service, method, resolve_caller_id(caller))
		end,
		is_peer = function(caller)
			return contracts.is_peer(resolve_caller_id(caller))
		end,
		allows_session_path = function(caller)
			return contracts.allows_session_path(resolve_caller_id(caller))
		end,
		validate = contracts.validate,
	}
end

return ContractsService
