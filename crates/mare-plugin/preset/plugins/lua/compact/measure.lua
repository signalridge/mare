-- compact.measure: token estimation, overhead, budgets and threshold calculation;
-- part of the `compact` plugin (plugins/compact.lua).
local M = {}

local function tokens(s)
	return mare.text.tokens(s)
end

local mare_lib = require("mare_lib")
local budget = require("compact.budget")
local msg_tokens, estimate = mare_lib.message_tokens, mare_lib.estimate_messages

function M.create(ctx, cfg, S)
	local m = {
		tokens = tokens,
		msg_tokens = msg_tokens,
		estimate = estimate,
	}

	--- what every request carries besides the messages: the system prompt and the tool
	--- declarations. Tool
	--- declarations can change in place, so count the actual definitions on every measurement.
	function m.overhead(model)
		if S.prepared and (not model or model.ref == S.prepared.model.ref) then
			return S.prepared.overhead, S.prepared.breakdown
		end
		if not model then
			model = ctx.llm:current()
		end
		model = S.target_model or model
		local system, defs = "", {}
		if S.svc.context then
			local sp, err = S.svc.context:system_prompt(model)
			if sp == nil and err ~= nil then
				error(err)
			end
			if type(sp) ~= "string" then
				error("context:system_prompt did not return a string")
			end
			system = sp
		end
		if S.svc.tools then
			local d, err = S.svc.tools:definitions(model)
			if d == nil and err ~= nil then
				error(err)
			end
			if type(d) ~= "table" then
				error("tools:definitions did not return a table")
			end
			defs = d
		end
		local breakdown = { system = tokens(system), tools = #defs > 0 and tokens(defs) or 0 }
		return breakdown.system + breakdown.tools, breakdown
	end

	--- usage = the larger of the provider-reported usage (only while it belongs to the current
	--- context version) and the local estimate: messages plus system prompt and
	--- tool declarations — after a compaction the provider's number is void, and the estimate
	--- alone must not leave those out
	function m.measure(msgs)
		local ag = ctx.agent
		local used = estimate(msgs or (S.transaction and S.transaction.messages) or ag:messages()) + m.overhead()
		if
			not S.transaction
			and S.svc.usage
			and S.usage_version == S.version
			and (not S.prepared or S.usage_model == S.prepared.model.ref)
		then
			used = math.max(used, S.svc.usage:context_tokens() or 0)
		end
		return used
	end

	--- The exact cost of the complete request, including messages and hook additions.
	function m.request_tokens(req)
		assert(type(req.system or "") == "string", "compaction: request system must be text")
		assert(type(req.messages) == "table", "compaction: request messages must be an array")
		local msg_count = #req.messages
		for k in pairs(req.messages) do
			if math.type(k) ~= "integer" or k < 1 or k > msg_count then
				error("compaction: request messages must be an array", 0)
			end
		end
		mare.json.encode_strict(req.tools or {})
		if req.output_schema ~= nil then
			mare.json.encode_strict(req.output_schema)
		end
		local breakdown =
			{ system = tokens(req.system or ""), tools = #(req.tools or {}) > 0 and tokens(req.tools) or 0 }
		local total = breakdown.system + breakdown.tools
		if req.output_schema ~= nil then
			total = total + tokens(req.output_schema)
		end
		for _, message in ipairs(req.messages) do
			mare.json.encode_strict(message)
			total = total + msg_tokens(message)
		end
		return total, breakdown
	end

	function m.changed(new, prepared_overhead, notify)
		S.version = S.version + 1 -- older usage is void until a request reports on this context
		if S.prepared then
			S.prepared.history = mare_lib.copy_deep(new)
		end
		if S.svc.usage then
			S.svc.usage:set_context(estimate(new) + (prepared_overhead or m.overhead()), { notify = notify ~= false })
		end
	end

	-- Observers run after the durable context/model publication, never inside the guard.
	-- Their failure cannot turn an already committed context into a discarded result.
	function m.notify(new)
		local ok, err = pcall(function()
			ctx.bus:emit("agent/messages-replaced", { messages = new })
			if S.svc.usage then
				ctx.bus:emit("usage/changed", S.svc.usage:totals())
			end
		end)
		if not ok then
			pcall(mare.log, "warn", "context committed; observer failed: " .. tostring(err))
		end
	end

	--- budgets(model) -> {limit, window, reserved, message, pressure} | nil, configuration error
	function m.validate_config()
		return budget.validate_config(cfg)
	end

	function m.budgets(model, request_max_tokens)
		if not model and S.prepared and request_max_tokens == nil then
			return S.prepared.budget
		end
		return budget.compute(cfg, model or S.target_model or ctx.llm:current(), request_max_tokens)
	end

	function m.threshold(model)
		if S.prepared and (not model or model.ref == S.prepared.model.ref) then
			return S.prepared.budget.limit
		end
		if S.target_model and (not model or model.ref == S.target_model.ref) then
			return S.target_limit
		end
		local b, err = m.budgets(model)
		return b and b.limit or nil, err
	end

	-- a model whose budgets don't work is a configuration error the moment it is selected
	function m.check_model(model)
		local _, err = m.budgets(model)
		if err then
			ctx.bus:emit("host/notice", { level = "error", text = err })
			if mare.ui.connected() then
				ui.notify(err, "error")
			end
		end
		return err
	end

	return m
end

return M
