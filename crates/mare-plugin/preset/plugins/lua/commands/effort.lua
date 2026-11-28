-- Reasoning effort, the command side (/effort, /model Tab, Alt+, / Alt+.): the
-- shortcut for the model's `effort` parameter (llm/params). Only the levels the model offers are
-- listed, an unsupported request says what it was adjusted to (never a silent clamp), and the
-- choice is remembered per model in state.json `model_params[ref].effort`; `session = true`
-- changes this session only.
local M = {}

--- the levels `model` offers, in its order; nil when it has no effort parameter. With `llm`
--- (the service) they come from the parameter definitions, else from the model's metadata.
function M.levels(model, llm)
	if not model then
		return nil
	end
	if llm and llm.param_list then
		for _, d in ipairs(llm:param_list(model)) do
			if d.id == "effort" then
				return d.list
			end
		end
		return nil
	end
	if not model.reasoning then
		return nil
	end
	return model.efforts or { "low", "medium", "high" }
end

--- remembered effort per model ref (state.json `model_params`), {} when none / unreadable
function M.remembered()
	local ok, st = pcall(mare.state.read)
	local out = {}
	if ok and type(st) == "table" then
		for ref, t in pairs(type(st.model_params) == "table" and st.model_params or {}) do
			if type(t) == "table" and t.effort ~= nil then
				out[ref] = t.effort
			end
		end
	end
	return out
end

--- the names of `model`'s parameters, for "it has no effort parameter" notes
local function param_names(ctx, m)
	local out = {}
	for _, d in ipairs(ctx:get("llm").param_list and ctx:get("llm"):param_list(m) or {}) do
		out[#out + 1] = d.id
	end
	return out
end

--- set(ctx, level, opts) -> the level in effect | nil, err. Trims and validates `level`, adjusts
--- it to the current model's nearest supported level with a notice, remembers it for the model
--- unless `opts.session`. opts: {session, quiet}
function M.set(ctx, level, opts)
	opts = opts or {}
	level = string.lower(string.match(tostring(level or ""), "^%s*(.-)%s*$"))
	local m = ctx:get("llm"):current()
	local levels = M.levels(m, ctx:get("llm"))
	local all = {}
	for _, v in ipairs(ctx:get("llm"):efforts() or {}) do
		all[#all + 1] = v
	end
	for _, v in ipairs(levels or {}) do
		if not table.find(all, v) then
			all[#all + 1] = v
		end
	end
	if not table.find(all, level) then
		return nil, ("unknown effort %q — one of %s"):format(level, table.concat(all, ", "))
	end
	if m and not levels then
		local others = param_names(ctx, m)
		if #others > 0 then
			return nil, ("%s has no effort parameter; its parameters: %s"):format(m.ref, table.concat(others, ", "))
		end
		return nil, ("%s has no reasoning effort to set"):format(m.ref)
	end
	local used = level
	if m and levels and not table.find(levels, level) then
		used = ctx:get("llm"):clamp_effort(m, level) or level
	end
	ctx:get("llm"):set_effort(used, { save = not opts.session })
	if not opts.quiet then
		local where = m and (" · " .. (m.name or m.id)) or ""
		local scope = opts.session and " (this session)" or ""
		if used ~= level then
			ui.notify(
				("%s doesn't support effort %s — using %s (supported: %s)"):format(
					m.ref,
					level,
					used,
					table.concat(levels or {}, ", ")
				),
				"warning"
			)
		else
			ui.notify(("Effort: %s%s%s"):format(used, where, scope))
		end
	end
	return used
end

--- step(ctx, dir): one supported level down (-1) or up (+1)
function M.step(ctx, dir)
	local m = ctx:get("llm"):current()
	local levels = M.levels(m, ctx:get("llm"))
	if not levels then
		ui.notify((m and m.ref or "This model") .. " has no reasoning effort to adjust", "info")
		return
	end
	local cur = ctx:get("llm"):effort()
	local at = table.find(levels, cur) or 1
	local nxt = levels[math.max(1, math.min(#levels, at + dir))]
	if nxt == cur then
		ui.notify(("Effort is already at its %s: %s"):format(dir < 0 and "lowest" or "highest", cur), "info")
		return
	end
	M.set(ctx, nxt)
end

--- after a model switch: restore the effort remembered for it, else say when the current
--- effort is adjusted to what the model supports (llm clamps per request)
function M.on_model_changed(ctx, m)
	local levels = M.levels(m, ctx:get("llm"))
	if not levels then
		return
	end
	local raw = ctx:get("llm"):raw_effort()
	local eff = ctx:get("llm"):effort()
	if raw and eff and raw ~= eff then
		-- recorded for /profile (llm.params_log) whether or not a UI is connected
		if ctx:get("llm").note_param then
			ctx:get("llm"):note_param(m, { param = "effort", from = raw, to = eff, kind = "switch" })
		end
		if ui.connected() then
			ui.notify(("%s doesn't support effort %s — using %s"):format(m.ref, raw, eff), "info")
		end
	end
end

return M
