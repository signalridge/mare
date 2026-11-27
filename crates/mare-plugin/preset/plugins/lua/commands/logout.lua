-- /logout: implementation of the command registered by plugins/commands.lua (the entry
-- layer); loaded on the first /logout. Runs in the commands plugin's env.
return function(ctx, args)
	local name = args ~= "" and args or nil
	if not name then
		local list = ctx:get("llm"):logged_in()
		if #list == 0 then
			ui.notify("Not logged in to any provider (nothing stored in auth.json)", "info")
			return
		end
		local items = {}
		for i, n in ipairs(list) do
			local p = ctx:get("llm"):provider(n)
			items[i] = { label = n, detail = p and p.label or nil }
		end
		local i = ui.select({ title = "Log out of", items = items })
		name = i and list[i]
		if not name then
			return
		end
	end
	ctx:spawn(function()
		local ok, res = pcall(ctx:get("llm").logout, ctx:get("llm"), name)
		if not ok then
			ui.notify("Logout failed: " .. tostring(res), "error")
			return
		end
		for _, w in ipairs(res.warnings) do
			ui.notify(w, "warning")
		end
		if #res.removed == 0 then
			ui.notify(name .. ": nothing stored to remove", "info")
		else
			ui.notify(("Logged out of %s (%s)"):format(name, table.concat(res.removed, ", ")), "success")
		end
		if #res.kept > 0 then
			ui.notify(
				("%s is still configured by %s — remove it there"):format(name, table.concat(res.kept, ", ")),
				"warning"
			)
		end
	end)
end
