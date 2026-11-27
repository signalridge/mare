-- /debug: implementation of the command registered by plugins/commands.lua (the entry
-- layer); loaded on the first /debug. Runs in the commands plugin's env.
return function(ctx)
	local lines = {}
	local log = mare.paths.log .. "/mare.log"
	lines[#lines + 1] = ui.Line({ ui.Span("log: " .. log):hl("MareAccent") })
	local window = mare.fs.read_window(log, { offset = 1, limit = 1 })
	if window then
		window = mare.fs.read_window(log, { offset = math.max(1, window.total - 99), limit = 100 })
	end
	if window then
		for _, line in ipairs(window.lines) do
			lines[#lines + 1] = ui.Line({ ui.Span(line):hl("MareMuted") })
		end
	else
		lines[#lines + 1] = ui.Line({ ui.Span("no log yet (" .. log .. ")"):hl("MareDim") })
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
	if #crashes > 0 then
		lines[#lines + 1] = ui.Line({ ui.Span(("crash reports: %d (newest first)"):format(#crashes)):hl("MareError") })
		for i = 1, math.min(5, #crashes) do
			local report = mare.paths.crashes .. "/" .. crashes[i] .. "/report.txt"
			local missing = mare.fs.exists(report) and "" or " (no report.txt)"
			local date = os.date("%Y-%m-%d %H:%M:%S", tonumber(crashes[i]) // 1000)
			lines[#lines + 1] = ui.Line({ ui.Span(date .. "  " .. report .. missing):hl("MareMuted") })
		end
		lines[#lines + 1] = ui.Gap()
		local report = mare.paths.crashes .. "/" .. crashes[1] .. "/report.txt"
		local body = mare.fs.read_window(report, { offset = 1, limit = 40 })
		for _, line in ipairs(body and body.lines or {}) do
			lines[#lines + 1] = ui.Line({ ui.Span(line):hl("MareMuted") })
		end
	else
		lines[#lines + 1] = ui.Line({ ui.Span("no crash reports (" .. mare.paths.crashes .. ")"):hl("MareDim") })
	end
	ui.overlay("Debug", ui.Lines(lines))
end
