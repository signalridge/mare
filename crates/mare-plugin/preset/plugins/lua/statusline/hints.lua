-- statusline.hints: instructional / contextual hint row (dim hints joined by " · "), in
-- priority order so the narrowest terminal keeps the most useful one. Key names come from the
-- live keymap (`mare.keymap.hint`), so a rebind changes the text; a hint whose action isn't
-- bound is left out (or shows the look's fallback key).
local M = {}

--- the shortest key bound to `action` in the input ("ctrl+s"), as shown in a hint
local function key_for(action, labels)
	local best
	for _, k in ipairs(mare.keymap.hint(action, "editor")) do
		if not best or #k < #best or (#k == #best and k < best) then
			best = k
		end
	end
	return best and ((labels or {})[best] or best) or nil
end
--- hints for the current state, most important first: { text, … }
--- st: { running = bool, queued = n, agents = n }; opts: the look
function M.list(st, opts, labels)
	local out = {}
	local function add(action, what, fallback)
		local k = key_for(action, labels) or fallback
		if k then
			out[#out + 1] = k .. " " .. what
		end
	end
	if st.running then
		add("interrupt", "to interrupt")
		if (st.queued or 0) > 0 then
			add("steer", "to steer now")
		end
	end
	-- sub-agents: the agents screen, only while the `agents` action has a key (the agents
	-- plugin binds ← on an empty input; unbound or rebound, the hint follows)
	if (st.agents or 0) > 0 then
		add("agents", "for agents")
	end
	if not st.running then
		if st.editor_empty ~= false then
			add("keymap-help", "shortcuts", opts.shortcuts_key or "/hotkeys")
		end
		add("permissions.cycle", "mode")
	end
	return out
end

return M
