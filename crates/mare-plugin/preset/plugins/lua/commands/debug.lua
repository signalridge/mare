-- /debug: implementation of the command registered by plugins/commands.lua (the entry
-- layer); loaded on the first /debug. Runs in the commands plugin's env.
return function(ctx)
	local lines, plain = {}, {}
	local function add(text, style)
		plain[#plain + 1] = text
		lines[#lines + 1] = ui.Line({ ui.Span(text):hl(style) })
	end
	local log = mare.paths.log .. "/mare.log"
	add("log: " .. log, "MareAccent")
	local window = mare.fs.read_window(log, { offset = 1, limit = 1 })
	if window then
		window = mare.fs.read_window(log, { offset = math.max(1, window.total - 99), limit = 100 })
	end
	if window then
		for _, line in ipairs(window.lines) do
			add(line, "MareMuted")
		end
	else
		add("no log yet (" .. log .. ")", "MareDim")
	end

	local crashes = {}
	for _, e in ipairs(mare.fs.list(mare.paths.crashes) or {}) do
		if e.kind == "dir" and string.match(e.name, "^%d+$") then
			crashes[#crashes + 1] = e.name
		end
	end
	table.sort(crashes, function(a, b)
		return tonumber(a) > tonumber(b)
	end)
	lines[#lines + 1] = ui.Gap()
	plain[#plain + 1] = ""
	if #crashes > 0 then
		add(("crash reports: %d (newest first)"):format(#crashes), "MareError")
		for i = 1, math.min(5, #crashes) do
			local report = mare.paths.crashes .. "/" .. crashes[i] .. "/report.txt"
			local missing = mare.fs.exists(report) and "" or " (no report.txt)"
			local date = os.date("%Y-%m-%d %H:%M:%S", tonumber(crashes[i]) // 1000)
			add(date .. "  " .. report .. missing, "MareMuted")
		end
		lines[#lines + 1] = ui.Gap()
		plain[#plain + 1] = ""
		local report = mare.paths.crashes .. "/" .. crashes[1] .. "/report.txt"
		local body = mare.fs.read_window(report, { offset = 1, limit = 40 })
		for _, line in ipairs(body and body.lines or {}) do
			add(line, "MareMuted")
		end
	else
		add("no crash reports (" .. mare.paths.crashes .. ")", "MareDim")
	end
	ui.overlay("Debug", ui.Lines(lines))
	if not (ui.connected and ui.connected()) then
		return table.concat(plain, "\n")
	end
end
