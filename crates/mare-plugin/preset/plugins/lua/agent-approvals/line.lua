-- agent-approvals.line: sub-agent approvals waiting their turn as one
-- line under the editor — `! approval needed: <child> (+N)` — while the host's
-- `agent/approval-queue { pending = [origin, …] }` lists any.
local M = {}

--- what the line calls a waiting request: its label (or agent id), and the run it belongs to
function M.label_of(o)
	local function s(k)
		local v = type(o) == "table" and o[k] or nil
		if type(v) == "string" and v ~= "" then
			return v
		end
	end
	local l = s("label") or s("agent_id") or "sub-agent"
	local r = s("run_id")
	return r and (l .. " · run " .. r) or l
end

local PRETTY = { left = "←", right = "→", up = "↑", down = "↓" }

--- the shortest input key bound to the `agents` action, as a hint shows it ("←"), or nil
function M.agents_key()
	local best
	for _, k in ipairs(mare.keymap.hint("agents", "editor")) do
		if not best or #k < #best or (#k == #best and k < best) then
			best = k
		end
	end
	return best and (PRETTY[best] or best) or nil
end

--- the line for `labels` (oldest first), or nil when nobody waits
function M.render(labels)
	local first = labels[1]
	if not first then
		return nil
	end
	local U = mare.ui
	local spans = {
		U.Span("! ", "MareNotifyWarn"):bold(),
		U.Span("approval needed: ", "MareMuted"),
		U.Span(first, "MareTitle"),
	}
	if #labels > 1 then
		spans[#spans + 1] = U.Span((" (+%d)"):format(#labels - 1), "MareDim")
	end
	spans[#spans + 1] = U.Span("  · answered in order", "MareDim")
	-- where to see them: the agents screen, only while its action has a key
	local key = M.agents_key()
	if key then
		spans[#spans + 1] = U.Span(" · " .. key .. " for agents", "MareDim")
	end
	return U.Line(spans):wrapped()
end

return M
