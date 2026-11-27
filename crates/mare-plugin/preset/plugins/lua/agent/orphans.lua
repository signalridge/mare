-- Tool calls left without a result, for the
-- `agent` plugin (the entry, plugins/agent.lua); loaded on first use.
-- `D` = {ctx, A (the agent's state), append (the persistence barrier for a message)}.
--
-- Every call ends with exactly one result in the session history: the missing ones are written
-- (through `append`, i.e. `session:persist_message`) when a crashed session is resumed, when a fork is
-- made, and before the next run; what can't be written there (a gap in the middle of an older
-- history) is only patched into the request copy.
return function(D)
	local ctx, A, append = D.ctx, D.A, D.append
	local M = {}

	--- Result text for a call that has no result, by its recorded state: never
	--- auto-replayed; side-effecting calls that may have started are `outcome_unknown` — the model
	--- must check the real state first.
	function M.text(call)
		-- judged by evidence, not declarations: only the persisted result
		-- proves completion. `completed` without a result, `started`, or no record at all
		-- (older session) all mean the call may have run.
		local st = A.tool_states[call.id]
		if st and st.state == "prepared" then
			return "interrupted: this call never started; it is safe to run it again if still needed."
		end
		local se = st and st.side_effect
		if call.name == "call_tool" and type(call.arguments) == "table" and type(call.arguments.name) == "string" then
			local inner_st = A.tool_states[call.id .. "/nested/1"]
			if inner_st and inner_st.state == "prepared" then
				return "interrupted: this call never started; it is safe to run it again if still needed."
			end
			se = (inner_st and inner_st.side_effect)
				or (ctx.tools.side_effect and ctx.tools:side_effect(call.arguments.name))
				or "external"
		elseif not se and ctx.tools.side_effect then
			se = ctx.tools:side_effect(call.name)
		end
		if (se or "external") ~= "read" then
			-- idempotent or not: never replayed automatically
			return "outcome_unknown: this call may or may not have completed, and processes it started may still be running; check the actual state before re-running it."
		end
		return "interrupted: this call did not finish; it is safe to run it again."
	end

	local function result(call)
		return {
			role = "tool_result",
			tool_call_id = call.id,
			tool_name = call.name,
			content = mare.json.array({ { type = "text", text = M.text(call) } }),
			is_error = true,
		}
	end

	--- Every tool call needs a result immediately following its assistant message, or the API
	--- rejects the request: missing results are synthesized, out-of-place settled results are
	--- placed next to their owning assistant message, and results without a matching call are
	--- dropped (remove_orphan_outputs). Request copy only; stored history is untouched.
	function M.repair(history)
		local calls, results_by_id = {}, {}
		for _, m in ipairs(history) do
			if m.role == "assistant" then
				for _, b in ipairs(m.content or {}) do
					if b.type == "tool_call" and not b.hosted and b.id then
						calls[b.id] = true
					end
				end
			end
		end
		for _, m in ipairs(history) do
			if
				m.role == "tool_result"
				and m.tool_call_id
				and calls[m.tool_call_id]
				and not results_by_id[m.tool_call_id]
			then
				results_by_id[m.tool_call_id] = m
			end
		end
		local out, emitted = {}, {}
		for _, m in ipairs(history) do
			if m.role ~= "tool_result" then
				out[#out + 1] = m
				if m.role == "assistant" then
					for _, b in ipairs(m.content or {}) do
						if b.type == "tool_call" and not b.hosted and b.id and not emitted[b.id] then
							emitted[b.id] = true
							out[#out + 1] = results_by_id[b.id] or result(b)
						end
					end
				end
			end
		end
		if #out ~= #history then
			return out
		end
		for i = 1, #history do
			if out[i] ~= history[i] then
				return out
			end
		end
		return history
	end

	--- Write missing results for every unanswered tool call in the loaded history (not only
	--- the last assistant message). Idempotent: any call that already has a persisted result
	--- gets nothing.
	function M.settle()
		local n = #A.messages
		local have = {}
		for i = 1, n do
			local m = A.messages[i]
			if m.role == "tool_result" and m.tool_call_id then
				have[m.tool_call_id] = true
			end
		end
		for i = 1, n do
			local m = A.messages[i]
			if m.role == "assistant" then
				for _, b in ipairs(m.content or {}) do
					if b.type == "tool_call" and not b.hosted and b.id and not have[b.id] then
						have[b.id] = true
						local r = result(b)
						r.timestamp = mare.task.now()
						append(r)
					end
				end
			end
		end
	end

	--- A loaded session settles its orphans at once, so they are in the file (and in any fork
	--- made from it) before anything else happens: after a resume of a session whose last run
	--- crashed (the session service says so, and only then is its file open for writing), and
	--- after a fork (written to the new file). Tree navigation never writes: the results of a
	--- call may be on another branch. Never while a run is going or session writes are failing.
	--- `why`: the reason `session/changed` gave ("resume" | "fork").
	function M.on_loaded(ev, why)
		if (ev and ev.navigated) or A.busy or #A.pending > 0 then
			return
		end
		if why == "resume" then
			local session = D.follow.session()
			local ok, info = pcall(function()
				return session and session:info()
			end)
			if not (ok and type(info) == "table" and info.crashed and not info.readonly) then
				return
			end
		elseif why ~= "fork" then
			return
		end
		M.settle()
	end

	return M
end
