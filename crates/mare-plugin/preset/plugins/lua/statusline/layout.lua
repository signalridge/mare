-- statusline.layout: the look-driven status bar (`mare.plugin("statusline", { layout = … })`,
-- statusline.looks). Builds the segments of both sides, then fits them to the terminal width
-- by walking the look's `drop` list (an id drops that segment, `id:short` swaps in its short
-- form). What still doesn't fit is cut by the core.
local M = {}

local function width(pieces)
	local w = 0
	for _, p in ipairs(pieces) do
		w = w + mare.text.width(p[1])
	end
	return w
end

--- side: ordered { id, seg } → pieces joined by the look's separator
local function join(side, L, state)
	local out = {}
	for _, it in ipairs(side) do
		local s = state[it.id]
		if s ~= "drop" then
			local pieces = (s == "short" and it.seg.short) or it.seg.full
			if #out > 0 then
				out[#out + 1] = L.sep == "space" and { "  ", nil } or { " · ", "MareSubtle" }
			end
			for _, p in ipairs(pieces) do
				out[#out + 1] = p
			end
		end
	end
	return out
end

--- fit(left, right, L, room) -> left pieces, right pieces
function M.fit(left, right, L, room)
	local state = {}
	local l, r = join(left, L, state), join(right, L, state)
	for _, step in ipairs(L.drop or {}) do
		-- the core puts two cells before the left side and at least one between the sides
		if width(l) + width(r) + 3 <= room then
			break
		end
		local id = string.match(step, "^([%w_]+):short$")
		if id then
			state[id] = "short"
		else
			id = string.match(step, "^[%w_]+$")
			state[id or ""] = "drop"
		end
		if id then
			l, r = join(left, L, state), join(right, L, state)
		end
	end
	if width(l) + width(r) + 3 > room then
		state.hints = "drop"
		l, r = join(left, L, state), join(right, L, state)
	end
	for i = #right, 1, -1 do
		if width(l) + width(r) + 3 <= room then
			break
		end
		state[right[i].id] = "drop"
		l, r = join(left, L, state), join(right, L, state)
	end
	return l, r
end

--- pieces → a Line (a piece: { text, style name, bold?, fg colour?, link? })
function M.line(pieces)
	local ui = mare.ui
	local spans = {}
	for _, p in ipairs(pieces) do
		local sp = ui.Span(p[1])
		if p[2] then
			sp = sp:hl(p[2])
		end
		if p[3] then
			sp = sp:bold()
		end
		if p[4] then
			sp = sp:fg(p[4])
		end
		if p[5] then
			sp = sp:link(p[5])
		end
		spans[#spans + 1] = sp
	end
	return ui.Line(spans)
end

--- create_renderer(ctx, cfg, P, L, label_cache_box?): returns `render(which)` for `"left"` / `"right"`.
--- Loaded lazily on the first status bar render so headless startups skip all layout/segment modules.
function M.create_renderer(ctx, cfg, P, L, label_cache_box)
	local ui = mare.ui
	local S = require("statusline.segments")(ctx, P, L, label_cache_box)
	local function build()
		local function side(ids)
			local out = {}
			for _, id in ipairs(ids or {}) do
				local f = S[id]
				local seg = f and f()
				if seg and seg.full and #seg.full > 0 then
					out[#out + 1] = { id = id, seg = seg }
				end
			end
			return out
		end
		local room = (ui.width and ui.width() or 80)
			- (cfg.layout_margin or 2)
			- (P.external_width and P.external_width() or 0)
		return M.fit(side(L.left), side(L.right), L, room)
	end
	-- Each side fits both (the drop order spans the two sides). When the caller brackets a
	-- paired left+right redraw with `pair_box.active = true`, the left render caches the
	-- fitted right pieces for the immediately following right render so layout runs once.
	return function(which, pair_box)
		if which == "right" and pair_box and pair_box.active and pair_box.right ~= nil then
			local p = pair_box.right
			pair_box.right = nil
			return #p > 0 and M.line(p) or nil
		end
		local l, r = build()
		if which == "left" and pair_box and pair_box.active then
			pair_box.right = r
		end
		local p = which == "left" and l or r
		return #p > 0 and M.line(p) or nil
	end
end

return M
