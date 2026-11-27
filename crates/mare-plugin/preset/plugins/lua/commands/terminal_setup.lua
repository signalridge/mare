-- /terminal-setup: implementation of the command registered by plugins/commands.lua; loaded on
-- first use. Runs in the commands plugin's env. What this terminal reports (the
-- startup probe, ui.caps) and, for the terminal in use, how to make Shift+Enter and Alt+Enter
-- reach mare. Nothing is
-- written to the terminal's own configuration: the snippets are shown to copy.
local GUIDES = {
	iterm = {
		name = "iTerm2",
		lines = {
			"Shift+Enter works once the terminal reports modified keys:",
			"Settings → Profiles → Keys → General → enable “Report keys using CSI u”.",
			"Option as Meta (for Alt+Enter / Alt+M …): Profiles → Keys → Left Option key → Esc+.",
		},
	},
	apple = {
		name = "Terminal.app",
		lines = {
			"Terminal.app has no extended key reporting: use Ctrl+J or \\ then Enter for a new line.",
			"Option as Meta: Settings → Profiles → Keyboard → “Use Option as Meta key”.",
		},
	},
	ghostty = {
		name = "Ghostty",
		lines = {
			"The kitty keyboard protocol is on by default: Shift+Enter works as is.",
			"Remove an old `keybind = shift+enter=text:\\n` (it sends Ctrl+J, not Shift+Enter).",
			"Alt+Backspace: `keybind = alt+backspace=text:\\x1b\\x7f`.",
		},
	},
	wezterm = {
		name = "WezTerm",
		lines = {
			"~/.wezterm.lua: `config.enable_kitty_keyboard = true`.",
			"macOS Option+Enter is fullscreen by default; forward it:",
			"  { key = 'Enter', mods = 'ALT', action = wezterm.action.SendString('\\x1b[13;3u') }",
		},
	},
	kitty = { name = "kitty", lines = { "Supported without configuration." } },
	alacritty = {
		name = "Alacritty",
		lines = {
			"~/.config/alacritty/alacritty.toml — forward Option+Enter on macOS:",
			'  [[keyboard.bindings]] key = "Enter" mods = "Alt" chars = "\\u001b\\r"',
		},
	},
	vscode = {
		name = "VS Code terminal",
		lines = {
			"keybindings.json — send Shift+Enter as CSI u:",
			'  { "key": "shift+enter", "command": "workbench.action.terminal.sendSequence",',
			'    "args": { "text": "\\u001b[13;2u" }, "when": "terminalFocus" }',
		},
	},
	windows = {
		name = "Windows Terminal",
		lines = {
			"Alt+Enter is fullscreen by default: unbind it in settings.json (actions) to use it in mare;",
			"mare also accepts Ctrl+Q to queue a follow-up on Windows.",
		},
	},
}

local function guide_for(c)
	local tp = string.lower((c.term_program ~= "" and c.term_program) or "")
	local term = string.lower(c.term or "")
	if tp:find("iterm") then
		return GUIDES.iterm
	elseif tp:find("apple_terminal") then
		return GUIDES.apple
	elseif tp:find("ghostty") or term:find("ghostty") then
		return GUIDES.ghostty
	elseif tp:find("wezterm") then
		return GUIDES.wezterm
	elseif term:find("kitty") then
		return GUIDES.kitty
	elseif term:find("alacritty") then
		return GUIDES.alacritty
	elseif tp:find("vscode") then
		return GUIDES.vscode
	elseif mare.env and mare.env("WT_SESSION") then
		return GUIDES.windows
	end
	return nil
end

return function(_ctx)
	local okc, c = pcall(ui.caps)
	c = okc and type(c) == "table" and c or {}
	local function row(k, v, style)
		return ui.Line({ ui.Span(("%-18s"):format(k)):hl("MareMuted"), ui.Span(tostring(v)):hl(style or "MareText") })
	end
	local term = (c.term_program and c.term_program ~= "" and c.term_program) or c.term or "unknown"
	local lines = {
		row("terminal", term .. ((c.xtversion and c.xtversion ~= "") and ("  (" .. c.xtversion .. ")") or "")),
		row(
			"keyboard protocol",
			c.kitty_keyboard and "on — Shift+Enter and Alt+Enter are distinct keys"
				or "off — Shift+Enter may arrive as plain Enter",
			c.kitty_keyboard and "MareSuccess" or "MareWarning"
		),
		row("multiplexer", (c.multiplexer and c.multiplexer ~= "") and c.multiplexer or "none"),
		row("hyperlinks", c.hyperlinks and "yes" or "no"),
		row("images", c.images or "none"),
		ui.Gap(),
	}
	local g = guide_for(c)
	lines[#lines + 1] = ui.Line({ ui.Span(g and g.name or "Your terminal"):hl("MareHeading") })
	for _, l in
		ipairs(g and g.lines or {
			"No specific guide for this terminal. A new line always works with Ctrl+J or \\ then Enter.",
		})
	do
		lines[#lines + 1] = ui.Paragraph(ui.Line({ ui.Span(l) }), 2)
	end
	if c.multiplexer == "tmux" then
		lines[#lines + 1] = ui.Gap()
		lines[#lines + 1] = ui.Line({ ui.Span("tmux"):hl("MareHeading") })
		for _, l in ipairs({
			"~/.tmux.conf: `set -s extended-keys on` and `set -as terminal-features 'xterm*:extkeys'`",
			"Ctrl+B is tmux's prefix: press it twice to send it to mare.",
		}) do
			lines[#lines + 1] = ui.Paragraph(ui.Line({ ui.Span(l) }), 2)
		end
	end
	lines[#lines + 1] = ui.Gap()
	lines[#lines + 1] = ui.Line({
		ui.Span("Nothing was changed. Conflicting bindings: /doctor · all keys: /hotkeys"):hl("MareHint"),
	})
	ui.overlay("Terminal setup", ui.Lines(lines))
end
