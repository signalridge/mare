-- /doctor: implementation of the command registered by plugins/commands.lua; loaded on first use
-- . Runs in the commands plugin's env. The checks `mare doctor` gathers from
-- plugins (the `doctor` event: credentials, MCP servers, packages, skills …) plus what the
-- running session knows (plugin problems, config warnings, the terminal), as an overlay.
-- Directory permissions and `--fix` stay with `mare doctor`.
return function(ctx)
	local checks = {}
	local function add(ok, name, detail, warn)
		checks[#checks + 1] = { ok = ok, name = name, detail = detail, warn = warn }
	end
	add(true, "version", "mare " .. tostring(mare.version))
	local m = ctx:get("llm") and ctx:get("llm"):current()
	add(m ~= nil, "model", m and m.ref or "none — /login, then /model")
	local problems = ctx.kernel:problems()
	add(
		#problems == 0,
		"plugins",
		#problems == 0 and (#ctx.kernel:plugins() .. " loaded") or table.concat(problems, "; ")
	)
	if mare.plugins and mare.plugins.list then
		for _, st in ipairs(mare.plugins.list()) do
			for _, dp in ipairs(st.problems or {}) do
				add(false, "declaration", ("[%s] %s: %s (%s)"):format(dp.reporter, dp.plugin, dp.message, dp.key), true)
			end
		end
	end
	local warns = mare.config_warnings and mare.config_warnings() or {}
	local clean_warns = {}
	for i, w in ipairs(warns) do
		clean_warns[i] = mare.text.redact(w)
	end
	add(
		#clean_warns == 0,
		"config",
		#clean_warns == 0 and "no warnings" or table.concat(clean_warns, "; "),
		#clean_warns > 0
	)
	local unbound = require("commands.keys").unbound_essential()
	if #unbound > 0 then
		local parts = {}
		for _, u in ipairs(unbound) do
			parts[#parts + 1] = ("%s (%s)"):format(u.action, u.context)
		end
		add(
			false,
			"unbound keymap actions",
			"no key bound for " .. table.concat(parts, ", ") .. " (Ctrl+C still cancels modals)",
			true
		)
	end
	if ui.caps then
		local okc, c = pcall(ui.caps)
		if okc and type(c) == "table" then
			local term = (c.term_program ~= "" and c.term_program) or c.term or "?"
			local mux = (c.multiplexer and c.multiplexer ~= "") and (" in " .. c.multiplexer) or ""
			local depth = tostring(ui.color_depth and ui.color_depth() or (c.truecolor and "truecolor" or "256"))
			local color = depth == "none" and "no color" or depth == "truecolor" and "true color" or (depth .. " color")
			add(
				true,
				"terminal",
				("%s%s · keyboard protocol %s · %s · images %s"):format(
					term,
					mux,
					c.kitty_keyboard and "on" or "off (/terminal-setup)",
					color,
					tostring(c.images or "none")
				)
			)
		end
	end
	mare.ui.working("Running checks")
	local plugin_checks = {}
	local ok, err = pcall(function()
		ctx.bus:emit("kernel/doctor", plugin_checks)
	end)
	mare.ui.working(nil)
	if not ok then
		add(false, "plugin checks", tostring(err))
	end
	for _, c in ipairs(plugin_checks) do
		add(c.ok == true, tostring(c.name or "plugin check"), tostring(c.detail or ""), c.warn == true)
	end
	local lines, failed = {}, 0
	for _, c in ipairs(checks) do
		local mark, style = "✓", "MareSuccess"
		if not c.ok then
			mark, style = c.warn and "!" or "✗", c.warn and "MareWarning" or "MareError"
			failed = failed + (c.warn and 0 or 1)
		end
		lines[#lines + 1] = ui.Paragraph(
			ui.Line({
				ui.Span(mark .. " "):hl(style),
				ui.Span(("%-16s "):format(c.name)):hl("MareAccent"),
				ui.Span(c.detail or ""):hl(c.ok and "MareMuted" or style),
			}),
			19
		)
	end
	lines[#lines + 1] = ui.Gap()
	lines[#lines + 1] = ui.Line({
		ui.Span(
			(failed == 0 and "All checks passed" or (failed .. " check(s) failed"))
				.. " · directory permissions and --fix: `mare doctor`"
		):hl("MareHint"),
	})
	if ui.connected() then
		ui.overlay("Doctor", ui.Lines(lines))
	elseif mare.mode == "print" or mare.mode == "tui" then
		-- Never print() plain text when stdout carries a protocol stream (rpc/acp/json/child).
		for _, c in ipairs(checks) do
			print(("%s %s: %s"):format(c.ok and "ok" or "FAIL", c.name, c.detail or ""))
		end
	end
	return checks
end
