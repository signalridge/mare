-- live preview/cancel/commit, backed by the public theme service.
return function(ctx, args)
	local theme = ctx:get("theme")
	if not theme then
		ui.notify("Theme service unavailable", "warning")
		return
	end
	if args ~= "" then
		local ok, err = pcall(theme.select, theme, args, { save = true })
		ui.notify(ok and ("Theme: " .. args) or tostring(err), ok and "info" or "warning")
		return
	end
	local before = theme:get()
	local names, items = { "auto" }, { { label = "auto", detail = "follow the terminal background" } }
	local selected = before.preference == "auto" and 1 or nil
	for _, d in ipairs(theme:list().data) do
		names[#names + 1] = d.name
		items[#items + 1] = { label = d.label, detail = d.name .. " · " .. d.origin }
		if d.name == before.preference then
			selected = #names
		end
	end
	local ok, i = pcall(ui.select, {
		title = "Theme",
		items = items,
		selected = selected or 1,
		on_highlight = function(idx)
			local snapshot = theme:preview(names[idx])
			return snapshot.component
		end,
	})
	theme:cancel_preview()
	if not ok then
		ui.notify("Theme preview failed: " .. tostring(i), "warning")
		return
	end
	if i then
		local saved, err = pcall(theme.select, theme, names[i], { save = true })
		ui.notify(saved and ("Theme: " .. names[i]) or tostring(err), saved and "info" or "warning")
	end
end
