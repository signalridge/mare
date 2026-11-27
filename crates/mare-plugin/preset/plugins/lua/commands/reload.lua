-- /reload: implementation of the command registered by plugins/commands.lua (the entry
-- layer); loaded on the first /reload. Runs in the commands plugin's env.
return function(ctx, args)
	if args == "config" and mare.reload_config then
		-- re-run init.lua, diff, reload only the affected plugins
		ctx:spawn(function()
			local ok, res = pcall(mare.reload_config)
			if not ok then
				ui.notify("Config reload failed: " .. tostring(res), "error")
				return
			end
			local changed = res.changed or {}
			local failures = res.failures or {}
			local reloaded = res.reloaded or {}
			ui.notify(
				#failures > 0 and "Config reloaded with plugin failures; check warnings"
					or #changed == 0 and #reloaded > 0 and ("Config reloaded: retried %s"):format(
						table.concat(reloaded, ", ")
					)
					or #changed == 0 and "Config reloaded: no changes"
					or ("Config reloaded: %s changed; reloaded %s"):format(
						table.concat(changed, ", "),
						#reloaded > 0 and table.concat(reloaded, ", ") or "no plugins"
					),
				#failures > 0 and "warning" or "success"
			)
			for _, failure in ipairs(failures) do
				ui.notify(failure, "error")
			end
			for _, w in ipairs(res.warnings or {}) do
				ui.notify(w, "warning")
			end
		end)
		return
	end
	local name = args ~= "" and args or nil
	if not name and mare.reload then
		-- full reload: init.lua again, new packages installed and loaded,
		-- removed ones unloaded, packages' config() again; reported by `reload/completed`
		-- (this plugin is reloaded too, so the new instance shows the summary)
		ctx:spawn(function()
			local ok, err = pcall(mare.reload)
			if not ok then
				ui.notify("Reload failed: " .. tostring(err), "error")
			end
		end)
		return
	end
	ctx:spawn(function()
		local ok, res = pcall(function()
			return ctx.kernel:reload(name)
		end)
		if ok then
			ui.notify(("Reloaded %d plugin(s)"):format(#res))
		else
			ui.notify("Reload failed: " .. tostring(res), "error")
		end
		for _, p in ipairs(ctx.kernel:problems()) do
			ui.notify(p, "warning")
		end
	end)
end
