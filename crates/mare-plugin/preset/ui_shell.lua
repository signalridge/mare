-- Default UI shell pieces: icon sets, color depth, file-type colors, the
-- startup logo, status-bar segments and the context gauge. Split out of ui_components.lua (module
-- size rule); loaded right after it by the same runtime, with the same `mare` argument, so users
-- override these exactly like the components there (`mare.ui.icons`, `mare.ui.Logo`, ...).

local mare = ...
local ui = mare.ui

---------------------------------------------------------------------------
-- interface shell: icons, file-type colors, status bar
-- segments with Powerline caps. All overridable: replace ui.icons[set][name], ui.filetypes, or
-- ui.Segments itself.
---------------------------------------------------------------------------
ui.icons = {
	nerd = {
		file = "\u{f15b}",
		dir = "\u{f07b}",
		command = "\u{f120}",
		branch = "\u{e0a0}",
		model = "\u{f2db}",
		jobs = "\u{f013}",
		workflow = "\u{f0e8}",
		info = "\u{f05a}",
		warn = "\u{f071}",
		error = "\u{f057}",
		sep_left = "\u{e0b6}",
		sep_right = "\u{e0b4}",
		pr = "\u{f407}",
		changes = "\u{f440}",
	},
	unicode = {
		file = "▪",
		dir = "▸",
		command = "›",
		branch = "⎇",
		model = "◆",
		jobs = "⚙",
		workflow = "⇶",
		info = "ℹ",
		warn = "⚠",
		error = "✖",
		pr = "⇄",
		changes = "±",
	},
	ascii = {
		file = "-",
		dir = "+",
		command = ">",
		branch = "@",
		model = "*",
		jobs = "&",
		workflow = "=",
		info = "i",
		warn = "!",
		error = "x",
		pr = "PR",
		changes = "+-",
	},
}

--- icon(name) -> glyph for the active icon set (mare.o.icons), "" if unknown
function ui.icon(name)
	local set = ui.icons[ui.icon_set and ui.icon_set() or "unicode"] or ui.icons.unicode
	return set[name] or ""
end

---------------------------------------------------------------------------
-- startup logo: pixel letters, drop shadow in the second accent color. Replace
-- `ui.Logo.bitmap` with your own (rows of "x" = pixel, anything else = empty), or override
-- `ui.Logo:render` / `ui.Logo:compact` entirely.
---------------------------------------------------------------------------
ui.Logo = {
	bitmap = {
		".xxxx...xxxx.. .xxxxxxx.. xx.xxxx. .xxxxxxx..",
		"xxxxxx.xxxxxx. xxxxxxxxx. xxxxxxxx xxxxxxxxx.",
		"xx..xxxxx..xx. ......xxx. xxx...xx xx.....xx.",
		"xx...xx....xx. .xxxxxxxx. xx...... xx.....xx.",
		"xx...xx....xx. xxxxxxxxx. xx...... xxxxxxxxx.",
		"xx...xx....xx. xx....xxx. xx...... xxxxxxxxx.",
		"xx...xx....xx. xx....xxx. xx...... xx........",
		"xx...xx....xx. xxxxxxxxx. xx...... xx.....xx.",
		"xx...xx....xx. .xxxxx.xxx xx...... xxxxxxxxx.",
		"xx...xx....xx. ........xx xx...... .xxxxxxx..",
	},
	min_width = 52,
}

local LOGO_STYLE =
	{ b = "MareLogo", s = "MareLogoShadow", bs = "MareLogoBodyOverShadow", sb = "MareLogoShadowOverBody" }

--- the bitmap as text rows: two pixel rows per row with ▀ ▄ █; the shadow is the body shifted one
--- pixel right and down, kept only where it lands on empty pixels (clipped to the bitmap height)
--- Key hints below the logo: { key, what? }.
ui.Logo.hints =
	{ { "/model" }, { "/help" }, { "@", "file" }, { "!", "bash" }, { "Shift+Tab", "mode" }, { "?", "keys" } }

local function logo_hints(hints, narrow)
	local ascii = ui.icon_set and ui.icon_set() == "ascii"
	local spans = {}
	for i, hint in ipairs(hints) do
		if not narrow or i <= 2 or i == #hints then
			if #spans > 0 then
				spans[#spans + 1] = ui.Span(ascii and " - " or " · "):hl("MareDim")
			end
			spans[#spans + 1] = ui.Span(hint[1]):hl("MareMuted")
			if hint[2] then
				spans[#spans + 1] = ui.Span(" " .. hint[2]):hl("MareDim")
			end
		end
	end
	return ui.Line(spans)
end

function ui.Logo:lines()
	local rows, h, w = self.bitmap, #self.bitmap, 0
	for _, r in ipairs(rows) do
		w = math.max(w, #r)
	end
	local function body(r, c)
		local row = rows[r]
		return row ~= nil and c >= 1 and c <= #row and row:sub(c, c) == "x"
	end
	local function px(r, c) -- nil | "b" | "s"
		if r > h then
			return nil
		end
		if body(r, c) then
			return "b"
		end
		if body(r - 1, c - 1) then
			return "s"
		end
		return nil
	end
	local out = {}
	for t = 1, h, 2 do
		local spans, cur_style, buf = {}, false, {}
		local function flush()
			if #buf > 0 then
				local sp = ui.Span(table.concat(buf))
				spans[#spans + 1] = cur_style and sp:hl(cur_style) or sp
				buf = {}
			end
		end
		for c = 1, w + 1 do
			local up, lo = px(t, c), px(t + 1, c)
			local ch, st
			if not up and not lo then
				ch, st = " ", nil
			elseif up == lo then
				ch, st = "█", LOGO_STYLE[up]
			elseif up and not lo then
				ch, st = "▀", LOGO_STYLE[up]
			elseif lo and not up then
				ch, st = "▄", LOGO_STYLE[lo]
			else
				ch, st = "▀", LOGO_STYLE[up .. lo]
			end
			if st ~= cur_style then
				flush()
				cur_style = st
			end
			buf[#buf + 1] = ch
		end
		flush()
		out[#out + 1] = ui.Line(spans)
	end
	return out
end

--- Compact form: narrow terminals, 16 colors, ascii icons, resumed sessions, logo = "compact".
--- info = { version, cwd, model }
function ui.Logo:compact(info)
	local ascii = ui.icon_set and ui.icon_set() == "ascii"
	local banner = ui.Line({
		ui.Span(ascii and "* " or "◆ "):hl("MareMark"),
		ui.Span("mare"):bold(),
		ui.Span(" " .. info.version):hl("MareDim"),
		ui.Span("  " .. info.cwd):hl("MareMuted"),
		ui.Span("  " .. (info.model or (ascii and "no model - /login" or "no model — /login"))):hl("MareDim"),
	})
	local width = ui.width and ui.width() or 80
	return ui.Lines({ banner, logo_hints(self.hints, width < 52) })
end

--- full form: the pixel logo, then `mare <version>  <cwd>`, then model and key hints
function ui.Logo:render(info)
	local items = self:lines()
	table.insert(items, 1, ui.Gap())
	items[#items + 1] = ui.Gap()
	items[#items + 1] = ui.Line({
		ui.Span("mare"):bold(),
		ui.Span(" " .. info.version):hl("MareDim"),
		ui.Span("  " .. info.cwd):hl("MareMuted"),
	})
	items[#items + 1] = ui.Line({ ui.Span(info.model or "no model — /login"):hl("MareDim") })
	items[#items + 1] = logo_hints(self.hints, false)
	return ui.Lines(items)
end

--- File-type styles (Yazi filetype): first matching rule wins; `dir` for paths ending in "/".
ui.filetypes = {
	{ pattern = "/$", style = "MareFileDir" },
	{
		pattern = "%.([%w]+)$",
		ext = {
			rs = "MareFileCode",
			lua = "MareFileCode",
			py = "MareFileCode",
			ts = "MareFileCode",
			tsx = "MareFileCode",
			js = "MareFileCode",
			go = "MareFileCode",
			c = "MareFileCode",
			h = "MareFileCode",
			cpp = "MareFileCode",
			java = "MareFileCode",
			rb = "MareFileCode",
			sh = "MareFileCode",
			zig = "MareFileCode",
			md = "MareFileDoc",
			txt = "MareFileDoc",
			rst = "MareFileDoc",
			toml = "MareFileConfig",
			json = "MareFileConfig",
			yaml = "MareFileConfig",
			yml = "MareFileConfig",
			lock = "MareFileConfig",
			ini = "MareFileConfig",
			png = "MareFileMedia",
			jpg = "MareFileMedia",
			jpeg = "MareFileMedia",
			gif = "MareFileMedia",
			svg = "MareFileMedia",
			webp = "MareFileMedia",
			zip = "MareFileArchive",
			gz = "MareFileArchive",
			tar = "MareFileArchive",
			xz = "MareFileArchive",
		},
	},
}

--- filetype_style(path) -> named style for a path (completion, read/edit summaries)
function ui.filetype_style(path)
	path = tostring(path or "")
	for _, rule in ipairs(ui.filetypes) do
		local m = string.match(path, rule.pattern)
		if m then
			if rule.ext then
				local st = rule.ext[string.lower(m)]
				if st then
					return st
				end
			else
				return rule.style
			end
		end
	end
	return "MareFileDefault"
end

--- file_icon(path) -> icon for a path (dir vs file)
function ui.file_icon(path)
	return ui.icon(string.sub(tostring(path or ""), -1) == "/" and "dir" or "file")
end

--- Segments(list, side) -> spans: Yazi-style status capsules. list = { {text=, style=} }; with a
--- Nerd Font each segment gets Powerline half-circle caps, otherwise padding + background.
function ui.Segments(list, side)
	local spans = {}
	local set = ui.icon_set and ui.icon_set() or "unicode"
	local caps = set == "nerd"
	local ascii = set == "ascii"
	local prev_bg = nil
	for _, seg in ipairs(list) do
		if seg.text and seg.text ~= "" then
			local spec = seg.style and ui.style(seg.style) or {}
			if #spans > 0 then
				-- capsules sit apart; plain text segments get a quiet separator
				spans[#spans + 1] = (spec.bg or prev_bg) and ui.Span(" ")
					or ui.Span(ascii and " - " or " · "):hl("MareSubtle")
			end
			prev_bg = spec.bg
			if spec.bg and caps then
				spans[#spans + 1] = ui.Span(ui.icon("sep_left")):fg(spec.bg)
				spans[#spans + 1] = ui.Span(seg.text):hl(seg.style)
				spans[#spans + 1] = ui.Span(ui.icon("sep_right")):fg(spec.bg)
			elseif spec.bg then
				spans[#spans + 1] = ui.Span(" " .. seg.text .. " "):hl(seg.style)
			else
				spans[#spans + 1] = ui.Span(seg.text):hl(seg.style or "MareStatus")
			end
		end
	end
	return spans
end

--- Gauge(fraction, width) -> "▰▰▰▱▱" (context usage bar)
function ui.Gauge(frac, width)
	width = width or 8
	local n = math.max(0, math.min(width, math.floor((frac or 0) * width + 0.5)))
	if ui.icon_set and ui.icon_set() == "ascii" then
		return string.rep("#", n) .. string.rep("-", width - n)
	end
	return string.rep("▰", n) .. string.rep("▱", width - n)
end
