-- llm: run the selected provider's `prepare` before a prompt's first request.
-- Loaded on the first prompt by llm/readiness. Runs in the llm plugin's env.
--
-- `prepare(model, {cancel})` returns normally when the provider is ready, or raises:
--   * an error with `retryable = true` (a token refresh that failed for a temporary reason:
--     network, 429, 5xx) is tried again a little — at most RETRIES more times, BACKOFF_MS apart
--     (a shorter `retry_after_ms` from the server is honoured) and only while the time spent
--     stays under BUDGET_MS — then the prompt goes on as before, the request using the cached
--     token and refreshing again itself;
--   * any other error refuses the prompt; `kind = "auth"` adds how to log in (only a 401 or a
--     known refresh-token code is permanent).
-- The backoff waits on the prompt's cancel token: Esc ends it at once and the agent stops the
-- prompt as cancelled.
local M = {}

M.RETRIES = 2
M.BACKOFF_MS = { 250, 1000 }
-- a server asking to wait longer than this isn't waited for here: the request retries itself
M.MAX_WAIT_MS = 2000
-- no retry starts once this much time went into preparing (a refresh that timed out after
-- 15 s isn't tried again before the prompt)
M.BUDGET_MS = 5000

local function message(err)
	return type(err) == "table" and tostring(err.message or err.kind) or tostring(err)
end

--- run(p, m, cancel, how_to_login) -> reason | nil: nil lets the prompt go on, a reason
--- refuses it.
function M.run(p, m, cancel, how_to_login)
	local started = mare.task.clock()
	local retries = 0
	while true do
		local ok, err = pcall(p.prepare, m, { cancel = cancel })
		if ok then
			return nil
		end
		if type(err) ~= "table" or err.retryable ~= true then
			local msg = message(err)
			-- a failure the user must log in again for says how
			if type(err) == "table" and err.kind == "auth" then
				msg = msg .. " — " .. how_to_login(m.provider)
			end
			return msg
		end
		retries = retries + 1
		local wait = tonumber(err.retry_after_ms) or M.BACKOFF_MS[retries] or M.BACKOFF_MS[#M.BACKOFF_MS]
		if retries > M.RETRIES or wait > M.MAX_WAIT_MS or mare.task.clock() - started + wait > M.BUDGET_MS then
			mare.log("warn", ("%s: preparing %s failed, sending anyway: %s"):format(m.provider, m.ref, message(err)))
			return nil
		end
		if not mare.task.sleep(wait, cancel) then
			return nil -- cancelled (Esc): the agent sees its token and stops the prompt
		end
	end
end

return M
