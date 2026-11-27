-- agent.service_events: dispose hook, service publication, and orphan recovery.
return function(S)
	local ctx, A, orphans, agent = S.ctx, S.A, S.orphans, S.agent

	-- Before reloading `agent` (or a provider whose reload restarts `agent`), cancel any active
	-- turn cooperatively and give in-flight tool execution a bounded grace window to unwind and
	-- emit closing item/step/turn completion events before fiber disposal aborts spawned tasks.
	local self_name = (ctx.fiber and ctx.fiber.name) or "agent"
	ctx:on("kernel/before-reload", function(ev)
		if not (ev and type(ev.plugins) == "table" and A.busy) then
			return
		end
		local reloading_self = false
		for _, id in ipairs(ev.plugins) do
			if id == self_name then
				reloading_self = true
				break
			end
		end
		if not reloading_self then
			return
		end
		if S.on_dispose then
			S.on_dispose()
		end
		if A.token then
			if not A.token:is_cancelled() then
				A.cancel_reason = { source = "disposed" }
			end
			A.token:cancel()
		end
		local deadline = mare.task.clock() + 5000
		while A.busy and mare.task.clock() < deadline do
			mare.task.sleep(10)
		end
	end)

	-- disposing the agent (e.g. shutdown or fallback) cancels the running turn and its tool tasks
	ctx:effect(function()
		return function()
			if S.on_dispose then
				S.on_dispose()
			end
			if A.token then
				if not A.token:is_cancelled() then
					A.cancel_reason = { source = "disposed" }
				end
				A.token:cancel()
			end
		end
	end, "agent running turn")

	-- a mare.Service
	require("mare_lib").service(ctx, "agent", agent)

	-- a resumed crashed session or a fork writes its missing tool results at once
	local load_reason -- `session/changed` names why the next `session/loaded` comes
	ctx:on("session/changed", function(ev)
		load_reason = ev and ev.reason
	end)
	ctx:on("session/loaded", function(ev)
		local why = load_reason
		load_reason = nil
		if why == "resume" or why == "fork" then
			orphans().on_loaded(ev, why)
		end
	end)
end
