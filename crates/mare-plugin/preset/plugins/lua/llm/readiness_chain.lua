-- llm.readiness_chain: the ordered request-readiness steps. `llm` owns
-- the list because agent, mcp and context can all reach it; order is a property of the list, not of plugin start order or event priorities.
--   register(caller, name, { order, prepare?, ready?, timeout_ms? }) -> dispose
--     a step is an effect of the caller's fiber; ascending `order`, equal orders in registration order
--   prepare(args): every step's `prepare(args)` in order, each awaited (with its timeout); the
--     first error stops the request
--   ready(args) -> vote | nil: every step's `ready(args)` in order; the first decision other than
--     "allow" is the result
local M = {}

function M.new()
	-- the shared ordered list (mare_lib.ordered): ascending order, equal orders in registration order
	local steps = require("mare_lib").ordered()

	local chain = {}

	function chain.register(caller, name, def)
		assert(type(name) == "string" and name ~= "", "readiness step needs a name")
		assert(type(def) == "table" and type(def.order) == "number", "readiness step needs a numeric order")
		local callable = require("mare_lib").callable
		assert(def.prepare == nil or callable(def.prepare), "readiness prepare must be a function")
		assert(def.ready == nil or callable(def.ready), "readiness ready must be a function")
		local step = { name = name, prepare = def.prepare, ready = def.ready, timeout_ms = def.timeout_ms }
		return caller:effect(function()
			return steps.add(def.order, step)
		end, "llm readiness " .. name)
	end

	function chain.prepare(args)
		steps.all(function(s)
			if not s.prepare then
				return
			end
			local limit = s.timeout_ms
			if type(limit) == "number" and limit > 0 then
				local finished = mare.task.timeout(limit, function()
					s.prepare(args)
				end)
				if not finished then
					error(("request preparation timed out: %s"):format(s.name), 0)
				end
			else
				s.prepare(args)
			end
		end)
	end

	function chain.ready(args)
		return steps.first(function(s)
			local vote = s.ready and s.ready(args)
			if type(vote) == "table" and vote.decision ~= nil and vote.decision ~= "allow" then
				return vote
			end
		end)
	end

	return chain
end

return M
