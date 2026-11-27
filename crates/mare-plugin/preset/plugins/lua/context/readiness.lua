-- context: the system prompt's readiness before a prompt's first request.
-- Loaded on the first prompt. Runs in the context plugin's env.
--
-- request/prepare (after the llm and MCP bands: the tool section lists what required MCP servers
-- registered) re-stats every instruction file and SKILL.md and drops the cached prompt when one
-- changed (the provider's prompt cache is invalidated once, recorded in /profile), then builds the
-- prompt for the selected model. A file or skill that can't be read is skipped with a notice (the
-- table in); a prompt that can't be built at all — a section or a
-- `context/system` listener raising — refuses the prompt in request/ready ("not sent: system
-- prompt could not be built: …") instead of failing after the prompt was taken.
-- Runs in the selected `context` provider (one slot per service: a replacement has its own).
local M = {}

--- M.new(env) -> {prepare = fn(event), ready = fn() -> vote | nil}
--- env: {stamp = fn() -> string, invalidate = fn(), build = fn(model) (the system prompt for
---       model, cached)}
function M.new(env)
	local stamp = nil
	local failed = nil -- why the prompt couldn't be built, for this prompt
	local R = {}

	function R.prepare(event)
		failed = nil
		local model = type(event) == "table" and event.model or nil
		for _ = 1, 4 do
			local before, before_cacheable = env.stamp()
			if not stamp or before ~= stamp or before_cacheable == false then
				env.invalidate()
				if stamp then
					mare.profile.mark("prompt cache invalidated: instruction files or skills changed")
				end
			end
			if not model then
				stamp = before_cacheable ~= false and before or nil
				return -- no model: the llm band refuses the prompt already
			end
			local ok, err = pcall(env.build, model)
			if not ok then
				failed = tostring(type(err) == "table" and (err.message or err) or err)
				return
			end
			local after, after_cacheable = env.stamp()
			if before == after then
				stamp = before_cacheable ~= false and after_cacheable ~= false and after or nil
				return
			end
			env.invalidate()
		end
		failed = "instruction files or skills changed while preparing the prompt; retry"
	end

	function R.ready()
		if failed then
			return { decision = "deny", reason = "system prompt could not be built: " .. failed }
		end
		return nil
	end

	return R
end

return M
