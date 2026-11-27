-- /add-dir: implementation of the command registered by plugins/commands.lua (the entry
-- layer); loaded on the first /add-dir. Runs in the commands plugin's env.
return function(ctx, args)
	if not mare.workspace then
		error("workspace support is not available")
	end
	if args == "" then
		ui.overlay(
			"Workspace",
			ui.Lines((function()
				local out = {}
				for _, d in ipairs(mare.workspace.dirs()) do
					out[#out + 1] = ui.Line({ ui.Span(d) })
				end
				return out
			end)())
		)
		return
	end
	local p = mare.fs.resolve(args)
	local sess = ctx:get("session")
	if sess and sess.add_workspace_dirs then
		sess:add_workspace_dirs({ p })
		ui.notify("Added to workspace: " .. p, "success")
		return
	end
	local st = mare.fs.stat(p)
	if not st or st.kind ~= "dir" then
		error("not a directory: " .. args)
	end
	local ok, err = pcall(mare.workspace.add, p)
	if not ok then
		error(tostring(err), 0)
	end
	ctx.bus:emit("workspace/changed", { dirs = mare.workspace.dirs() })
	ui.notify("Added to workspace: " .. p, "success")
end
