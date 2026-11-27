-- theme-default: assigns the named styles (Neovim highlight-group style). Restrained palette on
-- a tuned Catppuccin base: grays + accent + accent2. Override any style in init.lua:
--   mare.ui.style("MareAccent", { fg = "#7aa2f7" })
--   mare.setup { theme = { MareToolName = { fg = "#7aa2f7", bold = true }, MareDim = "#5c6370" } }
-- A theme package is just a plugin like this one (plus optional component overrides).
--
-- Flavors (Yazi): a dark and a light set. `mare.o.flavor = "auto"` (default) picks one from the
-- terminal background (OSC 11, queried after the first frame → event `ui/background`).

local function palette(p)
	return {
		MareText = {},
		MareAccent = { fg = p.accent },
		MareMuted = { fg = p.muted },
		MareDim = { fg = p.dim },
		MareSubtle = { fg = p.subtle },
		MareBorder = { fg = p.border },
		MareError = { fg = p.red },
		MareWarning = { fg = p.yellow },
		MareSuccess = { fg = p.green },
		MareInfo = { fg = p.blue },
		-- the user's message: a full-width bar in the selection color, default text color
		MareUserMessage = { bg = p.selection },
		MareUserPrompt = { fg = p.accent, bg = p.selection },
		MareAssistantMarker = {},
		MareReasoning = { fg = p.dim, italic = true },
		MareToolName = { bold = true },
		MareToolArgs = {},
		MareToolOk = { fg = p.green },
		MareToolError = { fg = p.red },
		MareToolPending = { fg = p.accent },
		MareToolResult = { fg = p.muted },
		MareToolGutter = { fg = p.dim },
		MareHeading = { fg = p.accent, bold = true },
		MareLink = { fg = p.blue },
		MareInlineCode = { fg = p.accent2 },
		MareCodeBlock = { bg = p.code_bg },
		MareQuote = { fg = p.muted },
		MareDiffAdd = { bg = p.add_bg },
		MareDiffDelete = { bg = p.del_bg },
		MareDiffAddText = { fg = p.green },
		MareDiffDeleteText = { fg = p.red },
		MareDiffAddWord = { bg = p.add_word },
		MareDiffDeleteWord = { bg = p.del_word },
		MareSelection = { bg = p.selection },
		MareSpinner = { fg = p.accent },
		-- the highlight sweeping the working label (mare.o.working_indicator = "cc" / "combined")
		MareSpinnerGlint = { fg = p.accent2 },
		MareStatus = { fg = p.muted },
		MareStatusDim = { fg = p.dim },
		MarePopupTitle = { fg = p.accent, bold = true },
		MarePopupSelected = { fg = p.accent, bold = true },
		MareHint = { fg = p.dim },
		-- interface shell
		MareBorderActive = { fg = p.accent, bold = true },
		MareBorderSearch = { fg = p.accent2, bold = true },
		MareTitle = { fg = p.accent, bold = true },
		MareSelectionInactive = { bold = true },
		MareKeyHint = { fg = p.accent },
		-- permission preset capsules: READ / AUTO / FULL / CUSTOM
		MareModeReadOnly = { fg = p.on_color, bg = p.green, bold = true },
		MareModeAuto = { fg = p.on_color, bg = p.blue, bold = true },
		MareModeFull = { fg = p.on_color, bg = p.red, bold = true },
		MareModeCustom = { fg = p.on_color, bg = p.yellow, bold = true },
		MareNotifyInfo = { fg = p.green, bold = true },
		MareNotifyWarn = { fg = p.yellow, bold = true },
		MareNotifyError = { fg = p.red, bold = true },
		MareScrollbar = { fg = p.muted },
		-- file types (Yazi filetype): completion items, read/edit summaries
		MareFileDir = { fg = p.blue, bold = true },
		MareFileCode = { fg = p.accent },
		MareFileDoc = { fg = p.text },
		MareFileConfig = { fg = p.yellow },
		MareFileMedia = { fg = p.accent2 },
		MareFileArchive = { fg = p.red },
		MareFileDefault = {},
		-- startup logo: body accent, drop shadow accent2; a cell whose halves differ
		-- is drawn as ▀ with fg = upper color, bg = lower color
		MareLogo = { fg = p.accent },
		MareLogoShadow = { fg = p.accent2 },
		MareLogoBodyOverShadow = { fg = p.accent, bg = p.accent2 },
		MareLogoShadowOverBody = { fg = p.accent2, bg = p.accent },
		MarePrompt = { fg = p.accent, bold = true },
		MareMark = { fg = p.accent },
		-- Markdown elements
		["md.h1"] = { fg = p.accent, bold = true }, -- no underline: it read as a link
		["md.h2"] = { fg = p.accent, bold = true },
		["md.h3"] = { fg = p.text, bold = true },
		["md.h4"] = { fg = p.muted, bold = true },
		["md.h5"] = { fg = p.muted, bold = true },
		["md.h6"] = { fg = p.muted, bold = true },
		["md.code"] = { fg = p.accent2, bg = p.code_bg },
		["md.code_block"] = { bg = p.code_bg },
		["md.code_edge"] = { fg = p.code_bg },
		["md.code_lang"] = { fg = p.dim },
		["md.code_line_nr"] = { fg = p.dim },
		["md.bullet"] = { fg = p.accent },
		["md.number"] = { fg = p.accent },
		["md.task_done"] = { fg = p.green },
		["md.task_todo"] = { fg = p.dim },
		["md.quote"] = { fg = p.muted },
		["md.quote_bar"] = { fg = p.border },
		["md.alert_note"] = { fg = p.blue },
		["md.alert_tip"] = { fg = p.green },
		["md.alert_important"] = { fg = p.accent },
		["md.alert_warning"] = { fg = p.yellow },
		["md.alert_caution"] = { fg = p.red },
		["md.table_header"] = { bold = true },
		["md.table_rule"] = { fg = p.border },
		["md.rule"] = { fg = p.border },
		["md.link"] = { fg = p.blue, underline = true },
		["md.link_url"] = { fg = p.dim },
		["md.path"] = { fg = p.accent2, bg = p.code_bg },
		["md.footnote"] = { fg = p.dim },
		["md.html"] = { fg = p.dim },
		["md.strike"] = { fg = p.dim },
		["md.image"] = { fg = p.dim },
	}
end

-- palette "薄暮": Catppuccin gray skeleton, mare's own accent (blue-violet) and
-- accent2 (pink-orange). Local backgrounds below are the fallback values computed against the
-- Catppuccin backgrounds #1e1e2e / #eff1f5; with a measured background they are re-blended.
local COLORS = {
	dark = {
		accent = "#a0a4ff",
		accent2 = "#ffa38f",
		text = "#cdd6f4",
		muted = "#b2b4d4",
		dim = "#8f91b1",
		subtle = "#707388",
		border = "#707199",
		selection = "#3f4062",
		code_bg = "#181825",
		green = "#9cd8b0",
		yellow = "#f0d38f",
		red = "#f57fa0",
		blue = "#85c1f5",
		add_bg = "#323c43",
		del_bg = "#402e40",
		add_word = "#465a58",
		del_word = "#633d52",
		on_color = "#141626",
	},
	light = {
		accent = "#5558d6",
		accent2 = "#d8604c",
		text = "#44475f", -- Catppuccin #4c4f69 darkened to ≥ 5 on the selected row
		muted = "#585b6f",
		dim = "#656879",
		subtle = "#828493",
		border = "#82839c",
		selection = "#bbbdea", -- 22% accent is under the 1.6 floor on #eff1f5; 34% reaches it
		code_bg = "#e5e7eb",
		green = "#2f8a5c",
		yellow = "#a0780f",
		red = "#c8406a",
		blue = "#2675c4",
		add_bg = "#d0e1dd",
		del_bg = "#e9d5df",
		add_word = "#b2d0c4",
		del_word = "#e3b8c9",
		on_color = "#ffffff",
	},
}

-- local backgrounds blended from the real terminal background (OSC 11 → ui/background {r,g,b}):
-- the terminal background and body text color are never set (MareText stays empty)
local function hex(c)
	return ("#%02x%02x%02x"):format(c[1], c[2], c[3])
end
local function rgb(h)
	return { tonumber(h:sub(2, 3), 16), tonumber(h:sub(4, 5), 16), tonumber(h:sub(6, 7), 16) }
end
local function mix(a, b, t) -- a + (b - a) * t
	return {
		math.floor(a[1] + (b[1] - a[1]) * t + 0.5),
		math.floor(a[2] + (b[2] - a[2]) * t + 0.5),
		math.floor(a[3] + (b[3] - a[3]) * t + 0.5),
	}
end

--- blend(colors, bg) -> colors with diff / selection / code backgrounds derived from bg
--- (16% green/red for diff lines, 32% for changed words, 25% (dark) / 34% (light) accent for
--- selection, bg darkened 20% (dark) / 4% (light) for code blocks), and the gray foregrounds
--- raised to their contrast floors against bg; replace this function in a theme
--- package
local RATIOS = {
	diff = 0.16,
	word = 0.32,
	selection_dark = 0.25,
	selection_light = 0.34,
	code_dark = 0.20,
	code_light = 0.04,
}

-- WCAG 2 contrast floors against the background; the order
-- text > muted > dim > subtle is kept. `selection` is the selected-row background against the
-- background, `on_selection` the body text on the selected row
local FLOORS = {
	dark = { text = 4.5, muted = 7, dim = 4.5, subtle = 3, border = 3, selection = 1.6, on_selection = 5 },
	light = { text = 4.5, muted = 5.5, dim = 4.5, subtle = 3, border = 3, selection = 1.6, on_selection = 5 },
}

local function luminance(c)
	local function ch(v)
		v = v / 255
		return v <= 0.03928 and v / 12.92 or ((v + 0.055) / 1.055) ^ 2.4
	end
	return 0.2126 * ch(c[1]) + 0.7152 * ch(c[2]) + 0.0722 * ch(c[3])
end
local function contrast(a, b)
	local la, lb = luminance(a), luminance(b)
	if la < lb then
		la, lb = lb, la
	end
	return (la + 0.05) / (lb + 0.05)
end
--- the same hue moved toward white (dark background) or black (light) just far enough to reach
--- `floor` against bg; unchanged when it already does
local function reach(c, bg, floor, toward)
	if contrast(c, bg) >= floor then
		return c
	end
	for i = 1, 50 do
		local m = mix(c, toward, i / 50)
		if contrast(m, bg) >= floor then
			return m
		end
	end
	return toward
end
local function enforce(out, bg, flavor)
	local floors = FLOORS[flavor] or FLOORS.dark
	local toward = flavor == "light" and { 0, 0, 0 } or { 255, 255, 255 }
	local prev = 1
	-- subtle → dim → muted → text: each at least its floor and above the previous level
	for _, k in ipairs({ "subtle", "dim", "muted", "text" }) do
		if out[k] then
			local c = reach(rgb(out[k]), bg, math.max(floors[k], prev + 0.05), toward)
			out[k] = hex(c)
			prev = contrast(c, bg)
		end
	end
	if out.border then
		out.border = hex(reach(rgb(out.border), bg, floors.border, toward))
	end
	-- body text on the selected row: moving it further from the background (same direction)
	-- also keeps it above muted
	if out.text and out.selection then
		out.text = hex(reach(rgb(out.text), rgb(out.selection), floors.on_selection, toward))
	end
end

--- the selected-row background: `ratio` of accent blended into bg, raised in 1% steps until it
--- stands out from bg by the selection floor (same hue, only the blend amount moves)
local function selection(bg, accent, ratio, flavor)
	local floor = (FLOORS[flavor] or FLOORS.dark).selection
	local t = ratio
	local c = mix(bg, accent, t)
	while contrast(c, bg) < floor and t < 1 do
		t = math.min(1, t + 0.01)
		c = mix(bg, accent, t)
	end
	return c
end

local function blend(c, bg, flavor, ratios)
	ratios = ratios or RATIOS
	local out = {}
	for k, v in pairs(c) do
		out[k] = v
	end
	local g, r = rgb(c.green), rgb(c.red)
	out.add_bg, out.del_bg = hex(mix(bg, g, ratios.diff)), hex(mix(bg, r, ratios.diff))
	out.add_word, out.del_word = hex(mix(bg, g, ratios.word)), hex(mix(bg, r, ratios.word))
	local sel = flavor == "light" and (ratios.selection_light or ratios.selection)
		or (ratios.selection_dark or ratios.selection)
	out.selection = hex(selection(bg, rgb(c.accent), sel, flavor))
	out.code_bg = hex(mix(bg, { 0, 0, 0 }, flavor == "light" and ratios.code_light or ratios.code_dark))
	enforce(out, bg, flavor)
	return out
end

-- Semantic palette fallback for packages that supply UI colors but no explicit syntax rules.
-- Custom tmTheme documents take precedence and retain their full scope selectors.
local function syntax(p)
	local parts = { '<?xml version="1.0"?><plist version="1.0"><dict><key>settings</key><array>' }
	local function rule(scope, color, font_style)
		parts[#parts + 1] = "<dict>"
			.. (scope and ("<key>scope</key><string>" .. scope .. "</string>") or "")
			.. "<key>settings</key><dict><key>foreground</key><string>"
			.. color
			.. "</string>"
			.. (font_style and ("<key>fontStyle</key><string>" .. font_style .. "</string>") or "")
			.. "</dict></dict>"
	end
	rule(nil, p.text)
	for _, entry in ipairs({
		{ "comment, punctuation.definition.comment", p.muted, "italic" },
		{
			"keyword, storage, keyword.control, keyword.operator.new, keyword.operator.expression, keyword.operator.logical, keyword.operator.word",
			p.accent,
		},
		{ "keyword.operator", p.accent2 },
		{
			"punctuation.definition.tag, punctuation.separator, punctuation.terminator, punctuation.accessor, punctuation.section",
			p.dim,
		},
		{ "string, markup.inline.raw.string", p.green },
		{ "string.regexp, constant.character.escape", p.accent2 },
		{ "constant.numeric, constant.language, constant.character, support.constant", p.yellow },
		{ "variable.parameter", p.yellow, "italic" },
		{ "variable.language, variable.other.constant", p.red },
		{ "entity.name.function, support.function, meta.function-call", p.blue },
		{
			"entity.name.type, entity.name.struct, entity.name.enum, entity.name.trait, entity.name.interface, entity.other.inherited-class, support.type, support.class",
			p.accent2,
		},
		{ "entity.name.namespace, entity.name.module", p.blue },
		{ "entity.name.tag, meta.tag.sgml", p.red },
		{ "entity.other.attribute-name", p.yellow },
		{ "invalid, invalid.deprecated", p.red },
		{ "markup.heading, entity.name.section", p.accent, "bold" },
		{ "markup.bold", p.text, "bold" },
		{ "markup.italic", p.text, "italic" },
		{ "markup.underline, markup.underline.link", p.blue, "underline" },
		{ "markup.quote", p.muted, "italic" },
		{ "markup.raw, markup.inline.raw", p.green },
		{ "markup.list", p.accent },
		{ "markup.inserted, diff.inserted", p.green },
		{ "markup.deleted, diff.deleted", p.red },
		{ "markup.changed, diff.changed", p.yellow },
		{ "meta.diff.range, meta.diff.header", p.accent },
	}) do
		rule(entry[1], entry[2], entry[3])
	end
	parts[#parts + 1] = "</array></dict></plist>"
	return table.concat(parts)
end
return { colors = COLORS, ratios = RATIOS, palette = palette, blend = blend, syntax = syntax }
