-- Hidden restoration is opt-in only for inputs the tool consumes as data, never as program text.
local M = {}
function M.observe(ctx)
	local changes = mare.projection.subscribe()
	ctx:effect(function()
		return function()
			changes:close()
		end
	end, "projection subscription")
	ctx:spawn(function()
		while true do
			local revision = changes:recv()
			if revision == nil then
				return
			end
			ctx.bus:emit("projection/changed", { revision = revision })
		end
	end)
end

function M.check(arguments, fields)
	local allowed = {}
	for _, name in ipairs(fields or {}) do
		allowed[name] = true
	end
	for name, value in pairs(arguments) do
		if mare.projection.contains(value) and not allowed[name] then
			return false, "Hidden values are not supported in this input; shell/program text cannot be restored safely"
		end
	end
	return true
end
return M
