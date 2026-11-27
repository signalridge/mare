-- Tracks parent turn/approval/input status for an open side conversation.
-- Only observes public agent/*, approval/*, and interaction/* events, ignoring side-owned calls.
return function(ctx, is_side_call, on_change, is_parent_busy)
	local function parent_busy()
		if is_parent_busy then
			return is_parent_busy()
		end
		local ag = ctx:get("agent")
		return ag and (ag:get() or {}).busy == true or false
	end

	local initial = parent_busy() and "working" or "finished"
	local state = {
		state = initial,
		detail = nil,
		pending_approvals = {},
		pending_inputs = {},
		pending_calls = {},
	}

	local function notify()
		if on_change then
			on_change()
		end
	end

	local function sync_idle_busy()
		if next(state.pending_inputs) ~= nil then
			state.state = "needs_input"
			local first = next(state.pending_inputs)
			state.detail = state.pending_inputs[first]
			return
		end
		if next(state.pending_approvals) ~= nil then
			state.state = "needs_approval"
			local first = next(state.pending_approvals)
			state.detail = state.pending_approvals[first]
			return
		end
		if parent_busy() then
			state.state = "working"
			state.detail = nil
		elseif state.state == "working" or state.state == "needs_approval" or state.state == "needs_input" then
			state.state = "finished"
			state.detail = nil
		end
	end

	ctx:on("turn/started", function()
		state.pending_approvals = {}
		state.pending_inputs = {}
		state.pending_calls = {}
		state.state = "working"
		state.detail = nil
		notify()
	end)

	ctx:on("item/started", function(ev)
		local item = type(ev) == "table" and ev.item or nil
		if type(item) ~= "table" or item.type ~= "tool_call" then
			return
		end
		local cid = item.tool_call_id or ev.item_id
		if cid and not is_side_call(cid) then
			state.pending_calls[cid] = item.name or "tool"
		end
	end)

	ctx:on("item/completed", function(ev)
		local item = type(ev) == "table" and ev.item or nil
		if type(item) ~= "table" or item.type ~= "tool_call" then
			return
		end
		local cid = item.tool_call_id or ev.item_id
		if cid then
			state.pending_calls[cid] = nil
		end
	end)

	ctx:on("approval/requested", function(ev)
		if type(ev) ~= "table" or is_side_call(ev.tool_call_id) then
			return
		end
		local key = tostring(ev.approval_id or ev.tool_call_id or "approval")
		state.pending_approvals[key] = tostring(ev.category or "tool")
		sync_idle_busy()
		notify()
	end)

	ctx:on("approval/resolved", function(ev)
		if type(ev) ~= "table" then
			return
		end
		local key = tostring(ev.approval_id or ev.tool_call_id or "")
		if key ~= "" then
			state.pending_approvals[key] = nil
		end
		sync_idle_busy()
		notify()
	end)

	ctx:on("interaction/requested", function(ev)
		if type(ev) ~= "table" or is_side_call(ev.tool_call_id) then
			return
		end
		local key = tostring(ev.interaction_id or ev.tool_call_id or "input")
		state.pending_inputs[key] = tostring(ev.kind or "input")
		sync_idle_busy()
		notify()
	end)

	ctx:on("interaction/resolved", function(ev)
		if type(ev) ~= "table" then
			return
		end
		local key = tostring(ev.interaction_id or ev.tool_call_id or "")
		if key ~= "" then
			state.pending_inputs[key] = nil
		end
		sync_idle_busy()
		notify()
	end)

	ctx:on("turn/completed", function(ev)
		state.pending_approvals = {}
		state.pending_inputs = {}
		state.pending_calls = {}
		if type(ev) == "table" then
			if ev.termination_reason == "interrupted" or ev.status == "interrupted" then
				state.state = "interrupted"
				state.detail = nil
			elseif ev.error then
				state.state = "failed"
				state.detail = require("mare_lib").error_message(ev.error)
			else
				state.state = "finished"
				state.detail = nil
			end
		else
			state.state = "finished"
			state.detail = nil
		end
		notify()
	end)

	ctx:on("agent/error", function(ev)
		state.pending_approvals = {}
		state.pending_inputs = {}
		state.state = "failed"
		state.detail = type(ev) == "table" and ev.error and tostring(ev.error) or nil
		notify()
	end)

	local M = {}

	function M.get()
		sync_idle_busy()
		return {
			state = state.state,
			detail = state.detail,
		}
	end

	return M
end
