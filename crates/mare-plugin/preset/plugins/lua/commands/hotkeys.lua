-- /hotkeys: implementation of the command registered by plugins/commands.lua (the entry
-- layer); loaded on the first /hotkeys. Runs in the commands plugin's env.
-- Every binding with its description, plus the editor's own keys (commands.keys).
return function(_ctx)
	local lines = require("commands.keys").lines(18)
	lines[#lines + 1] = ui.Gap()
	lines[#lines + 1] = ui.Line({
		ui.Span("Ctrl+O toggles expanded output; Ctrl+T opens the transcript. Configure keys in init.lua.")
			:hl("MareDim"),
	})
	ui.overlay("Key bindings", ui.Lines(lines))
end
