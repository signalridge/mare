-- /effort [level]: implementation of the command registered by
-- plugins/commands.lua; loaded on first use. Runs in the commands plugin's env.
-- The picker lists only the levels the current model supports; `/effort <level>` adjusts an
-- unsupported level to the nearest one and says so (commands.effort).
return function(ctx, args)
	local E = require("commands.effort")
	if not ctx:get("llm") then
		ui.notify("No model service", "warning")
		return false, "No model service"
	end
	local session = false
	local a = args
	if a == "--session" or string.find(a, "^%-%-session%s") then
		session = true
		a = string.match(a, "^%-%-session%s*(.*)$")
	end
	if a ~= "" then
		local _, err = E.set(ctx, a, { session = session })
		if err then
			ui.notify(err, "warning")
			return false, err
		end
		return
	end
	local m = ctx:get("llm"):current()
	local levels = E.levels(m, ctx:get("llm"))
	if not levels then
		local others = {}
		for _, d in ipairs(m and ctx:get("llm").param_list and ctx:get("llm"):param_list(m) or {}) do
			others[#others + 1] = d.id
		end
		if #others > 0 then
			ui.notify(
				("%s has no effort parameter; its parameters: %s"):format(m.ref, table.concat(others, ", ")),
				"info"
			)
			return
		end
		ui.notify((m and m.ref or "No model selected") .. ": no reasoning effort to set", "info")
		return
	end
	local cur = ctx:get("llm"):effort()
	local items, sel = {}, 1
	for i, e in ipairs(levels) do
		items[i] = { label = e, detail = e == cur and "current" or nil }
		if e == cur then
			sel = i
		end
	end
	local i, _, key = ui.select({
		title = "Reasoning effort · " .. m.ref,
		items = items,
		selected = sel,
		keys = { { key = "ctrl+s", label = "this session only" } },
	})
	if i then
		E.set(ctx, levels[i], { session = session or key == "ctrl+s" })
	end
end
