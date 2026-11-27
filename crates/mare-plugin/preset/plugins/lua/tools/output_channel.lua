-- tools.output_channel: active-execution streaming output channels (`tools:output_channel` /
-- `tools:send_output`), shared equally by built-in and third-party tools and orchestrators.
return function(D)
	local ctx, tools = D.ctx, D.registry.tools
	local lib = require("mare_lib")
	local active_executions = {}
	local channels = {}

	local function close_entry(entry)
		if entry.closed then
			return
		end
		entry.closed = true
		if type(entry.handle) == "table" then
			entry.handle._mare_closed_output_channel = true
		end
		channels[entry.handle] = nil
		entry.on_output = nil
		local dispose = entry.dispose
		entry.dispose = nil
		if dispose then
			pcall(dispose)
		end
	end

	local function close_all()
		local list = {}
		for _, entry in pairs(channels) do
			list[#list + 1] = entry
		end
		for _, entry in ipairs(list) do
			close_entry(entry)
		end
	end

	ctx:on("session/changed", close_all)
	ctx:effect(function()
		return close_all
	end, "tools output channels")

	local function begin_execution(tool_call_id, spec)
		local stack = active_executions[tool_call_id]
		if not stack then
			stack = {}
			active_executions[tool_call_id] = stack
		end
		stack[#stack + 1] = spec
		local closed = false
		return function()
			if not closed then
				closed = true
				local cur_stack = active_executions[tool_call_id]
				if cur_stack then
					for i = #cur_stack, 1, -1 do
						if cur_stack[i] == spec then
							table.remove(cur_stack, i)
							break
						end
					end
					if #cur_stack == 0 then
						active_executions[tool_call_id] = nil
					end
				end
			end
		end
	end

	function tools.output_channel(_, caller)
		local cur = mare.exec and mare.exec.current and mare.exec.current() or nil
		local call_id = type(cur) == "table" and cur.tool_call_id or nil
		local stack = call_id and active_executions[call_id] or nil
		local spec = stack and stack[#stack] or nil
		if not spec or type(spec.on_output) ~= "function" then
			return nil, "active_output_unavailable"
		end
		local owner = lib.caller_id(caller) or lib.caller_id(ctx)
		local entry = {
			owner = owner,
			execution_id = spec.execution_id,
			tool_call_id = spec.tool_call_id,
			session_id = spec.session_id,
			session_epoch = spec.session_epoch,
			on_output = spec.on_output,
			closed = false,
		}
		local handle = {
			close = function()
				close_entry(entry)
			end,
		}
		entry.handle = handle
		channels[handle] = entry
		entry.dispose = lib.owned(caller or ctx, function()
			entry.dispose = nil
			close_entry(entry)
		end, "tool output channel")
		return handle
	end

	local function validate_blocks(blocks)
		if type(blocks) ~= "table" then
			return false
		end
		local dense, count = lib.is_dense_list(blocks)
		if not dense or count == 0 then
			return false
		end
		for i = 1, count do
			local b = blocks[i]
			if type(b) ~= "table" or type(b.type) ~= "string" or b.type == "" then
				return false
			end
		end
		return true
	end

	function tools.send_output(_, caller, channel, blocks, opts)
		local entry = channel ~= nil and channels[channel] or nil
		if not entry then
			if type(channel) == "table" and channel._mare_closed_output_channel then
				return { status = "rejected", error = "channel_closed" }
			end
			return { status = "rejected", error = "invalid_channel" }
		end
		if entry.closed then
			return { status = "rejected", error = "channel_closed" }
		end
		local caller_id = lib.caller_id(caller) or lib.caller_id(ctx)
		if caller_id ~= entry.owner then
			return { status = "rejected", error = "channel_owner_mismatch" }
		end
		if entry.session_id ~= nil and D.svc.session then
			local cur_sess = D.svc.session:info()
			if cur_sess.session_id ~= entry.session_id or cur_sess.session_epoch ~= entry.session_epoch then
				close_entry(entry)
				return { status = "rejected", error = "session_changed" }
			end
		end
		local cancel = type(opts) == "table" and opts.cancel or nil
		if cancel and cancel.is_cancelled and cancel:is_cancelled() then
			return { status = "rejected", error = "cancelled" }
		end
		if not validate_blocks(blocks) then
			return { status = "rejected", error = "invalid_blocks" }
		end
		local proj_ok, projected = pcall(mare.projection.project, blocks, { lossy = true })
		if not proj_ok or type(projected) ~= "table" then
			return { status = "rejected", error = "projection_failed" }
		end
		local ok, receipt = pcall(entry.on_output, projected, {
			cancel = cancel,
			tool_call_id = entry.tool_call_id,
			execution_id = entry.execution_id,
		})
		if not ok then
			return { status = "rejected", error = tostring(receipt) }
		end
		if type(receipt) == "table" and receipt.status == "accepted" then
			return { status = "accepted" }
		end
		local reason = (type(receipt) == "table" and type(receipt.error) == "string" and receipt.error ~= "")
				and receipt.error
			or "output_rejected"
		return { status = "rejected", error = reason }
	end

	return {
		begin_execution = begin_execution,
	}
end
