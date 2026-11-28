-- agent.loop_tools: tool call batching (parallel concurrency-safe + serialized barriers) and result commits.
return function(D)
	local ctx, cfg, A = D.ctx, D.cfg, D.A
	local persist_message = D.persist.persist_message
	local publish_message = D.persist.publish_message
	local tool_calls, result_message = D.tool_exec.tool_calls, D.tool_exec.result_message
	local tool_state, run_tool = D.tool_exec.tool_state, D.tool_exec.run_tool

	local function run_one(call, truncated, token, declared_tools, step_id, started_calls, output_gate)
		if truncated then
			-- the arguments may be cut off mid-way
			return {
				content = "Not run: the response hit the output token limit, so this call's arguments may be incomplete. Retry with a smaller step.",
				is_error = true,
			}
		elseif call.parse_error then
			-- never run with silently-empty arguments
			return {
				content = "Not run: the arguments are not valid JSON ("
					.. tostring(call.parse_error)
					.. "): "
					.. mare.text.cut(tostring(call.raw_arguments or ""), 500),
				is_error = true,
			}
		elseif token:is_cancelled() then
			return { content = "Aborted by user.", is_error = true, cancelled = true }
		elseif A.halt then
			return {
				content = "Not run: " .. tostring(A.halt),
				is_error = true,
				error_code = A.persist_error and "persist_failed" or nil,
			}
		elseif #A.steer > 0 then
			return { content = "Skipped: the user sent a new message.", is_error = true }
		end
		return run_tool(call, token, declared_tools, {
			step_id = step_id,
			started_calls = started_calls,
			output_gate = output_gate,
		})
	end

	local function resolve_status(res)
		if res._mare_result_status then
			return res._mare_result_status
		end
		if res.error_code == "persist_failed" then
			return "persist_failed"
		elseif res.refused then
			return "denied"
		elseif res.cancelled then
			return "cancelled"
		end
		return res.is_error and "error" or "success"
	end

	local function to_item_status(result_status)
		if result_status == "success" then
			return "completed"
		elseif result_status == "denied" then
			return "denied"
		elseif result_status == "cancelled" then
			return "cancelled"
		elseif result_status == "persist_failed" then
			return "persist_failed"
		end
		return "failed"
	end

	local function tool_result_meta(res)
		if type(res) ~= "table" then
			return nil
		end
		local out = {}
		local any = false
		for k, v in pairs(res) do
			if k ~= "content" and k ~= "details" and not (type(k) == "string" and k:sub(1, 6) == "_mare_") then
				if k == "truncated" then
					out.output_truncated = out.output_truncated or v
				else
					out[k] = v
				end
				any = true
			end
		end
		return any and out or nil
	end

	--- Persist one completed tool result and its completion state marker to the session journal
	--- immediately, returning a persisted record for ordered in-memory and event publication.
	local function persist_result(call, res)
		local rm = result_message(call, res)
		local persisted, persist_err = persist_message(rm)
		local completion_persisted = false
		local completion_checked = false
		if persisted then
			local st = A.tool_states[call.id]
			if st and st.state == "started" then
				completion_checked = true
				completion_persisted = tool_state(call, "completed", st) == true
			end
		end
		return {
			res = res,
			rm = persisted and persisted.msg or rm,
			persisted = persisted,
			persist_err = persist_err,
			completion_checked = completion_checked,
			completion_persisted = completion_persisted,
		}
	end

	local function publish_result(call, k, record, results, step_id, started_calls, output_gate)
		local res = record.res
		local rm = record.rm
		local appended, append_error, message_persisted, result_entry_id
		if record.persisted then
			appended, append_error, message_persisted, result_entry_id = publish_message(record.persisted)
		else
			appended, append_error, message_persisted = nil, record.persist_err, false
		end
		if not appended then
			if output_gate then
				output_gate.closed = true
				output_gate.error = "initial_result_failed"
			end
			error(append_error or "tool result could not be committed")
		end
		results[k] = rm
		local status = resolve_status(res)
		-- one logical commit: the result, then the completion marker (both already persisted)
		if record.completion_checked and not record.completion_persisted then
			status = "persist_failed"
		end
		if not message_persisted and not record.completion_persisted then
			status = "persist_failed"
		end
		if output_gate then
			if status == "persist_failed" or status == "denied" or status == "cancelled" then
				output_gate.closed = true
				output_gate.error = "initial_result_" .. status:gsub("-", "_")
			else
				output_gate.committed = true
			end
		end
		local item_status = to_item_status(status)
		if not (started_calls and started_calls[call.id]) then
			ctx.bus:emit("item/started", {
				turn_id = A.turn_id,
				item_id = call.id,
				step_id = step_id,
				item = {
					type = "tool_call",
					tool_call_id = call.id,
					provider_call_id = call.provider_call_id,
					name = call.name,
					arguments = call.arguments,
					status = "pending",
				},
			})
		end
		local live_changes = type(res) == "table" and type(res._mare_changes) == "table" and res._mare_changes or nil
		ctx.bus:emit("item/completed", {
			turn_id = A.turn_id,
			item_id = call.id,
			status = item_status,
			item = {
				type = "tool_call",
				tool_call_id = call.id,
				provider_call_id = call.provider_call_id,
				name = call.name,
				arguments = call.arguments,
				status = item_status,
				result = tool_result_meta(res),
				message = rm,
				changes = live_changes,
				result_status = status,
				execution_status = "unknown",
				result_entry_id = result_entry_id,
			},
		})
	end

	local function is_concurrency_safe(call)
		if ctx.tools.concurrency_safe then
			return ctx.tools:concurrency_safe(call)
		end
		return ctx.tools:readonly(call.name)
	end

	local function run_parallel_batch(calls, i, j, exec_fn, publish_fn)
		local raw_tx, raw_rx = mare.task.channel()
		local ready_tx, ready_rx = mare.task.channel()
		local batch, next_launch, next_commit, running = {}, i, i, 0
		local total = j - i + 1
		local function launch()
			local k = next_launch
			next_launch = next_launch + 1
			running = running + 1
			-- tasks belong to the plugin's ctx: disposing the agent cancels them
			ctx:spawn(function()
				local ok, res = pcall(exec_fn, calls[k])
				raw_tx:send({
					k = k,
					res = ok and res or { content = "Tool error: " .. tostring(res), is_error = true },
				})
			end)
		end
		-- Dedicated serial journal consumer: persists each completed sibling result and launches
		-- queued siblings independently of ordered `item/completed` listeners, so a slow or hung
		-- completion listener on an earlier call never delays journaling later completed calls.
		local consumer = ctx:spawn(function()
			for _ = 1, total do
				local d = raw_rx:recv()
				running = running - 1
				if next_launch <= j then
					launch()
				end
				local record = persist_result(calls[d.k], d.res)
				ready_tx:send({ k = d.k, record = record })
			end
		end)
		local cleanup_consumer <close> = setmetatable({}, {
			__close = function()
				consumer:abort()
			end,
		})
		local limit = math.max(1, tonumber(cfg.max_parallel) or 10)
		while next_launch <= j and running < limit do
			launch()
		end
		while next_commit <= j do
			local d = ready_rx:recv()
			batch[d.k] = d.record
			while batch[next_commit] do
				publish_fn(next_commit, batch[next_commit])
				next_commit = next_commit + 1
			end
		end
	end

	-- consecutive concurrency-safe calls (decided per call from its arguments) overlap, at most
	-- cfg.max_parallel at a time; anything else is an exclusive barrier and runs alone. Results
	-- are persisted as they finish and published one by one in the model's order.
	local function dispatch_calls(msg, truncated, token, decl_holder, step_id)
		local calls = tool_calls(msg)
		local results = mare.json.array()
		local declared_tools = decl_holder.tools
		local started_calls = {}
		local output_gates = {}
		local function gate_for(call)
			local g = output_gates[call.id]
			if not g then
				g = { committed = false, closed = false, error = nil, session_id = A.session_id }
				output_gates[call.id] = g
			end
			return g
		end
		local function exec_call(call)
			return run_one(call, truncated, token, declared_tools, step_id, started_calls, gate_for(call))
		end
		-- the batch ends the run when every result says so
		local terminating = 0
		local function publish_call(k, record)
			if type(record.res) == "table" and record.res.terminate == true then
				terminating = terminating + 1
			end
			publish_result(calls[k], k, record, results, step_id, started_calls, output_gates[calls[k].id])
		end
		local i = 1
		while i <= #calls do
			local j = i
			if not truncated and is_concurrency_safe(calls[i]) then
				while j + 1 <= #calls and is_concurrency_safe(calls[j + 1]) do
					j = j + 1
				end
			end
			if j == i then
				publish_call(i, persist_result(calls[i], exec_call(calls[i])))
			else
				run_parallel_batch(calls, i, j, exec_call, publish_call)
			end
			i = j + 1
		end
		return calls, results, #calls > 0 and terminating == #calls
	end

	return { dispatch_calls = dispatch_calls }
end
