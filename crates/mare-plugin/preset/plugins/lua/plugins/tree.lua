-- Runtime tree shown by /plugins tree or /plugins list.
return function(ctx)
	local lines = {}
	for _, p in ipairs(ctx.kernel:plugins()) do
		lines[#lines + 1] = ui.Line({
			ui.Span(p.active and "● " or "○ "):hl(p.active and "MareSuccess" or "MareDim"),
			ui.Span(("%-24s"):format(p.name)),
			ui.Span(p.origin):hl("MareMuted"),
		})
		-- `override = true` replaced a same-name definition
		if p.overrides then
			local what = p.overrides == "builtin" and "built-in" or (p.overrides .. " plugin")
			lines[#lines]:push(ui.Span(("  overrides %s %s"):format(what, p.name)):hl("MareWarning"))
		end
	end
	-- plugins stuck loading, unloading or waiting past `stall_ms` (observed, not aborted)
	for _, s in ipairs(ctx.kernel:stalled()) do
		lines[#lines + 1] = ui.Line({ ui.Span("… " .. s.summary):hl("MareWarning") })
	end
	-- every recorded failure with its traceback (one Line per line), until a full /reload clears it
	for _, pr in ipairs(ctx.kernel:problems()) do
		local first = true
		for l in (tostring(pr) .. "\n"):gmatch("([^\n]*)\n") do
			lines[#lines + 1] = ui.Line({ ui.Span((first and "! " or "  ") .. l:gsub("\t", "  ")):hl("MareWarning") })
			first = false
		end
	end
	ui.overlay("Plugins", ui.Lines(lines))
end
