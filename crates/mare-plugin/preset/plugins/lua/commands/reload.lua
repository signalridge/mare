-- /reload: implementation of the command registered by plugins/commands.lua (the entry
-- layer); loaded on the first /reload. Runs in the commands plugin's env.
local function busy_notice(cmd_label, err)
	local msg = type(err) == "table" and err.code == "busy" and tostring(err.message or "") or nil
	if not msg then
		return nil
	end
	if msg:find("model request is active", 1, true) then
		return ("%s isn't available while a model request is active (Esc stops it)"):format(cmd_label)
	end
	if msg:find("reload already in progress", 1, true) then
		return ("%s isn't available while a reload is already in progress; retry afterwards"):format(cmd_label)
	end
	return ("%s isn't available right now: %s"):format(cmd_label, msg)
end

local function clean_error(err)
	local msg = require("mare_lib").error_message(err) or "unknown error"
	msg = msg:match("^(.-)%s*stack traceback:") or msg
	while msg:find("^runtime error: ") do
		msg = msg:sub(16)
	end
	return (msg:gsub("%s+$", ""))
end

local function notify_failure(cmd_label, fail_prefix, err)
	local busy = busy_notice(cmd_label, err)
	if busy then
		ui.notify(busy, "warning")
		return
	end
	ui.notify(fail_prefix .. clean_error(err), "error")
end

return function(ctx, args)
	if args == "config" and mare.reload_config then
		-- re-run init.lua, diff, reload only the affected plugins
		ctx:spawn(function()
			local ok, res = pcall(mare.reload_config)
			if not ok then
				notify_failure("/reload config", "Config reload failed: ", res)
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
				notify_failure("/reload", "Reload failed: ", err)
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
			notify_failure(name and ("/reload " .. name) or "/reload", "Reload failed: ", res)
		end
		for _, p in ipairs(ctx.kernel:problems()) do
			ui.notify(p, "warning")
		end
	end)
end
