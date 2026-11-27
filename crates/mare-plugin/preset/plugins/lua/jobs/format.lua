-- jobs.format: clock, job ownership and the status / elapsed texts; part of the `jobs` plugin (plugins/jobs.lua), loaded at the start of its apply.
local function now()
	return mare.task.now()
end

-- jobs belong to the agent that started them: a child agent's VM carries its
-- id in mare.agent_id (set by mare.subagent.start), the main session is "main"
local function owner_of(tctx)
	-- a tool context carries agent_id; a caller ctx (a cordis ctx, where reading an undeclared
	-- property raises) never does
	local ok, id = pcall(function()
		return tctx and tctx.agent_id
	end)
	return (ok and id) or mare.agent_id or "main"
end

local function status_text(job)
	local s = job.status
	if job.stop_unconfirmed then
		-- couldn't confirm the process tree ended: say so
		return s .. ", stop_unconfirmed"
	end
	if job.exit_code ~= nil and s ~= "running" then
		s = s .. (" (exit %d)"):format(job.exit_code)
	elseif job.exit_signal and s ~= "running" then
		s = s .. (" (signal %d)"):format(job.exit_signal)
	end
	return s
end

local function elapsed(job)
	local ms = (job.ended_at_ms or now()) - job.started_at_ms
	if ms < 60000 then
		return ("%ds"):format(ms // 1000)
	end
	return ("%dm%02ds"):format(ms // 60000, (ms // 1000) % 60)
end

return { now = now, owner_of = owner_of, status_text = status_text, elapsed = elapsed }
