-- ui.working_draw: turns a working-line look (ui.working_looks) into nodes the core animates —
-- ui.Frames for the glyph and the label, a dim tail — for mare.ui.Working:render. Colours come
-- from the theme's named styles; `info.color` picks blended RGB (truecolor), three intensity
-- levels of the named styles (256 colours:71-80) or bold only (NO_COLOR).
-- `info.still` (reduce_motion: the core draws only still frames): each part is its still node,
-- with no animation frames built.
local ui = mare.ui
local tips = require("ui.tips")
local M = {}

local function rgb(hex)
	if type(hex) ~= "string" then
		return nil
	end
	local r, g, b = string.match(hex, "^#(%x%x)(%x%x)(%x%x)$")
	return r and { tonumber(r, 16), tonumber(g, 16), tonumber(b, 16) } or nil
end

local function fg(name)
	local ok, s = pcall(ui.style, name)
	return ok and type(s) == "table" and s.fg or nil
end

--- a span of `text` at intensity t (0..1) between the styles `base` and `peak`
local function shade(text, base, peak, t, color)
	local a, b = rgb(fg(base)), rgb(fg(peak))
	if color == "truecolor" and a and b then
		local function m(i)
			return math.floor(a[i] + (b[i] - a[i]) * math.max(0, math.min(1, t)) + 0.5)
		end
		return ui.Span(text):fg(("#%02x%02x%02x"):format(m(1), m(2), m(3)))
	end
	if t >= 0.6 then
		return ui.Span(text):hl(peak):bold()
	end
	return ui.Span(text):hl(t >= 0.2 and peak or base)
end
M.shade = shade

local function chars(s)
	local out = {}
	for _, c in utf8.codes(s) do
		out[#out + 1] = utf8.char(c)
	end
	return out
end

--- one glyph frame: flat (one colour) or shaded accent → accent2 across its cells
local function glyph_line(s, flat, color, tint)
	local base = tint or "MareSpinner"
	if flat then
		return ui.Line({ ui.Span(s .. " "):hl(base) })
	end
	local cs = chars(s)
	local spans = {}
	for i, c in ipairs(cs) do
		spans[#spans + 1] = shade(c, base, "MareSpinnerGlint", (i - 1) / math.max(1, #cs - 1), color)
	end
	spans[#spans + 1] = ui.Span(" ")
	return ui.Line(spans)
end

function M.glyph(L, data, info)
	local g, color = L.glyph, info.color
	local ascii = ui.icon_set and ui.icon_set() == "ascii"
	if g == "none" or g == nil then
		return nil
	end
	local custom_glyphs = L.glyphs or data.frames
	local d = (L.frames and L.frames.frames and L.frames) or custom_glyphs[g] or data.frames[g]
	if d and type(d.render) == "function" then
		return d.render(info)
	end
	if g == "bullet" and not d then
		-- nothing under reduced motion; a bold • on truecolor, else •/◦ every 600 ms
		local empty = ui.Line({})
		local filled, hollow = ascii and "*" or "•", ascii and "." or "◦"
		if info.still then
			return empty
		end
		if color == "truecolor" then
			return ui.Frames({ ui.Line({ ui.Span(filled .. " "):hl("MareSpinner"):bold() }) }, 1000, empty)
		end
		return ui.Frames({
			ui.Line({ ui.Span(filled .. " "):hl("MareSpinner") }),
			ui.Line({ ui.Span(hollow .. " "):hl("MareSpinner") }),
		}, 600, empty)
	end
	if g == "pulse" and not d then
		local dot = ascii and "*" or "●"
		local still = ui.Line({ ui.Span(dot .. " "):hl("MareSpinner") })
		if color == "none" or info.still then
			return still
		end
		local frames = {}
		for i = 0, 23 do
			local t = 0.5 - 0.5 * math.cos(2 * math.pi * i / 24)
			frames[#frames + 1] = ui.Line({ shade(dot, "MareDim", "MareSpinner", t, color), ui.Span(" ") })
		end
		return ui.Frames(frames, 100, still)
	end
	if not d or not d.frames or #d.frames == 0 then
		return nil
	end
	local tint = d.tint
	if d.ascii and ascii then
		d = d.ascii
	end
	local still = glyph_line(d.still or d.frames[1], d.flat, color, tint)
	if info.still then
		return still
	end
	local frames = {}
	for _, f in ipairs(d.frames) do
		frames[#frames + 1] = glyph_line(f, d.flat, color, tint)
	end
	local hold = type(d.hold) == "number" and math.floor(d.hold) or 0
	if hold > 0 and #frames > 0 then
		local last = frames[#frames]
		for _ = 1, hold do
			frames[#frames + 1] = last
		end
	end
	return ui.Frames(frames, L.interval_ms or d.ms or 120, still)
end

--- the intensity of cell i at position pos of a sweep (nil pos: resting)
local function band(S, i, pos)
	if not pos then
		return 0
	end
	local d = pos - i
	if S.shape == "crest" then
		if d >= 0 then
			return math.exp(-d / S.half)
		end
		return d > -1 and 1 + d or 0
	end
	local x = math.abs(d)
	return x <= S.half and 0.5 * (1 + math.cos(math.pi * x / S.half)) or 0
end

function M.label(L, data, info)
	local text = info.label .. "…"
	local still = ui.Line({ ui.Span(text):hl("MareSpinner") })
	local l, color = L.label, info.color
	if info.still then
		return still
	end
	if l == "dots" then
		local frames = {}
		for n = 0, 3 do
			frames[#frames + 1] = ui.Line({
				ui.Span(info.label):hl("MareSpinner"),
				ui.Span(string.rep("·", n) .. string.rep(" ", 3 - n)):hl("MareSpinner"),
			})
		end
		return ui.Frames(frames, 400, still)
	end
	local S = data.sweeps[l]
	if not S then
		return still
	end
	local cs = chars(text)
	local len = #cs + 2 * S.pad
	local n = math.max(1, math.floor((S.run + S.rest) * 1000 / S.tick + 0.5))
	local frames = {}
	for f = 0, n - 1 do
		local t = f * S.tick / 1000
		local pos = t < S.run and (t / S.run * len - S.pad) or nil
		local spans = {}
		for i, c in ipairs(cs) do
			spans[#spans + 1] = shade(c, S.base, S.peak, band(S, i - 1, pos), color)
		end
		frames[#frames + 1] = ui.Line(spans)
	end
	return ui.Frames(frames, S.tick, still)
end

--- `59s`, `1m 05s`, `1h 02m 05s`
local function format_elapsed(s)
	if s < 60 then
		return s .. "s"
	elseif s < 3600 then
		return ("%dm %02ds"):format(s // 60, s % 60)
	end
	return ("%dh %02dm %02ds"):format(s // 3600, s % 3600 // 60, s % 60)
end

function M.tail(L, info)
	local parts = {}
	local el = L.elapsed
	if el ~= "off" and el ~= false then
		if el == "seconds" then
			parts[#parts + 1] = info.secs .. "s"
		else
			parts[#parts + 1] = format_elapsed(info.secs)
		end
	end
	if L.tokens and info.detail then
		parts[#parts + 1] = info.detail
	end
	if L.interrupt_hint ~= false and L.interrupt_hint ~= "off" and info.interrupt_key then
		parts[#parts + 1] = info.interrupt_key .. " to interrupt"
	end
	for _, h in ipairs(info.hints or {}) do
		parts[#parts + 1] = h
	end
	if #parts == 0 then
		return nil
	end
	local sep = L.sep == "bullet" and " • " or " · "
	return ui.Line({ ui.Span(" (" .. table.concat(parts, sep) .. ")"):hl("MareDim") })
end

local function stopping_glyph(L, data, info)
	local sp = L.stopping_spinner
	if sp == false or sp == "none" then
		return nil
	elseif type(sp) == "string" then
		return M.glyph({ glyph = sp, glyphs = L.glyphs, interval_ms = L.interval_ms }, data, info)
	elseif type(sp) == "table" then
		return M.glyph(
			{ glyph = sp.glyph or "custom", frames = sp, glyphs = L.glyphs, interval_ms = sp.ms or L.interval_ms },
			data,
			info
		)
	end
	return M.glyph(L, data, info)
end

function M.tip(L, info)
	if info.stopping or L.tips == false then
		return nil
	end
	local text = (type(info.tip) == "string" and info.tip ~= "" and info.tip) or tips.pick(info.secs, L.tips)
	if not text then
		return nil
	end
	local sep = L.sep == "bullet" and " • " or " · "
	return ui.Line({ ui.Span(" " .. sep .. text):hl("MareDim") })
end

--- the whole line for `info` (handles both active working state and `info.stopping`)
function M.row(L, data, info)
	if info.stopping then
		local g = stopping_glyph(L, data, info)
		local hint = ui.Line({ ui.Span(info.stop_hint or "stopping… (Ctrl+C again to force)"):hl("MareDim") })
		if g then
			return ui.Row({ g, hint })
		end
		return hint
	end
	-- any part may be nil (no glyph, no tail, no tip): keep the others in order
	local parts = { M.glyph(L, data, info), M.label(L, data, info), M.tail(L, info), M.tip(L, info) }
	local items = {}
	for i = 1, 4 do
		if parts[i] then
			items[#items + 1] = parts[i]
		end
	end
	return ui.Row(items)
end

return M
