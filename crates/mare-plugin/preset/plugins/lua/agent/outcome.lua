-- agent.outcome: structured termination reasons for agent runs.
local M = {}

--- Map structured provider/model error_kind to a stable termination reason.
function M.model_error(kind)
	if kind == "usage_limit" or kind == "quota" then
		return "usage_limit"
	end
	return "error"
end

--- Build a failed run result shape when execution throws or fails before completion.
function M.failure(error, messages)
	return {
		text = "",
		messages = messages or mare.json.array(),
		usage = {},
		error = tostring(error),
		termination_reason = "error",
	}
end

return M
