-- ui.working: the working line's elements from `mare.o.ui.working` (part of the `ui`
-- plugin). The built-in glyph/sweep tables live in `ui.working_looks` (exposed read-only as
-- `mare.ui.Working.frames` / `.sweeps`); `ui.working_draw` turns the resolved options into nodes;
-- the core only animates them.
--
-- Configure any individual leaf in `mare.setup { ui = { working = { ... } } }`:
--   mare.o.ui.working = {
--     glyph = "ember", label = "glint", elapsed = "auto", tokens = true,
--     interrupt_hint = true, sep = "dot", tools = "still", text_throttle_ms = 250,
--     verbs = { reasoning = "Thinking", responding = "Responding", tool = "Working", shell = "Running bash", exploring = "Exploring" },
--   }
local M = {}
local data = require("ui.working_looks")
local draw = require("ui.working_draw")

local KEYS = {
	"glyph",
	"label",
	"elapsed",
	"tokens",
	"interrupt_hint",
	"tips",
	"sep",
	"tools",
	"interval_ms",
	"text_throttle_ms",
}

local function value(k, v)
	if v == "on" or v == "true" then
		return true
	elseif v == "off" or v == "false" then
		return false
	elseif k == "interval_ms" or k == "text_throttle_ms" then
		return tonumber(v) or v
	end
	return v
end

local function read_only_view(tbl, label)
	if type(tbl) ~= "table" then
		return tbl
	end
	local cache = {}
	return setmetatable({}, {
		__metatable = false,
		__index = function(_, k)
			local v = tbl[k]
			if type(v) == "table" then
				local c = cache[k]
				if not c then
					c = read_only_view(v, label)
					cache[k] = c
				end
				return c
			end
			return v
		end,
		__newindex = function()
			error((label or "mare.ui.Working") .. " is a read-only view of ui.working configuration", 2)
		end,
		__pairs = function()
			local k
			return function()
				local nk, nv = next(tbl, k)
				k = nk
				if nk == nil then
					return nil
				end
				if type(nv) == "table" then
					local c = cache[nk]
					if not c then
						c = read_only_view(nv, label)
						cache[nk] = c
					end
					return nk, c
				end
				return nk, nv
			end,
				nil,
				nil
		end,
		__len = function()
			return #tbl
		end,
	})
end

local function closest(needle, map)
	if type(needle) ~= "string" then
		return nil
	end
	local best, best_d
	local n = #needle
	for k in pairs(map) do
		local m = #k
		local prev, cur = {}, {}
		for j = 0, m do
			prev[j] = j
		end
		for i = 1, n do
			cur[0] = i
			local ai = needle:byte(i)
			for j = 1, m do
				local cost = ai == k:byte(j) and 0 or 1
				cur[j] = math.min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + cost)
			end
			prev, cur = cur, prev
		end
		local d = prev[m]
		if d <= math.max(2, math.floor(n / 2)) and (not best_d or d < best_d) then
			best, best_d = k, d
		end
	end
	return best
end

--- the look for an option value (`ui.working`)
local function raw_ui_section(o)
	if o == nil then
		o = mare.o or mare.config
	end
	if rawequal(o, mare.o) then
		return type(mare.config) == "table" and type(mare.config.ui) == "table" and mare.config.ui or nil
	end
	return type(o) == "table" and type(o.ui) == "table" and o.ui or nil
end

function M.resolve(o)
	local ui_opt = raw_ui_section(o)
	local v = ui_opt and ui_opt.working or nil
	local resolved, err = mare.config_resolve_preset({
		defaults = data.defaults,
		presets = data.presets,
		default_preset = "combined",
	}, v)
	if err or type(resolved) ~= "table" then
		if err then
			mare.log("warn", "ui.working: " .. tostring(err))
		end
		local fallback_v = {}
		if type(v) == "table" then
			for k, x in pairs(v) do
				if k ~= "preset" and k ~= "presets" then
					fallback_v[k] = x
				end
			end
		end
		resolved = mare.config_resolve_preset({
			defaults = data.defaults,
			presets = data.presets,
			default_preset = "combined",
		}, fallback_v)
	end
	local out = {}
	for k, x in pairs(resolved or data.defaults) do
		out[k] = value(k, x)
	end
	local custom_glyphs
	if type(v) == "table" then
		for _, k in ipairs(KEYS) do
			if v[k] ~= nil then
				out[k] = value(k, v[k])
			end
		end
		out.frames = v.frames
		if type(v.glyphs) == "table" then
			custom_glyphs = v.glyphs
		end
	end
	if custom_glyphs then
		local merged_glyphs = {}
		for gk, gv in pairs(data.frames) do
			merged_glyphs[gk] = gv
		end
		for gk, gv in pairs(custom_glyphs) do
			merged_glyphs[gk] = gv
		end
		out.glyphs = merged_glyphs
	end
	if out.elapsed == false then
		out.elapsed = "off"
	end
	local glyph_map = out.glyphs or data.frames
	if
		out.glyph
		and out.glyph ~= "none"
		and out.glyph ~= "bullet"
		and out.glyph ~= "pulse"
		and not (out.frames and out.frames.frames)
		and not glyph_map[out.glyph]
	then
		local near = closest(out.glyph, glyph_map)
		mare.log(
			"warn",
			("unknown working glyph `%s`%s"):format(
				tostring(out.glyph),
				near and (" — did you mean `" .. near .. "`?") or ""
			)
		)
		out.glyph = "ember"
	end
	if ui_opt and type(ui_opt.stopping) == "table" and ui_opt.stopping.spinner ~= nil then
		out.stopping_spinner = ui_opt.stopping.spinner
	end
	return out
end

--- install the look for `o` on mare.ui.Working (read-only view) and configure tool_call/stopping spinners
local active_proxy = nil
local active_views = nil

local function install_working_views(glyph_map)
	local U = mare.ui
	local frames_view = read_only_view(glyph_map, "mare.ui.Working.frames")
	local sweeps_view = read_only_view(data.sweeps, "mare.ui.Working.sweeps")
	if active_proxy ~= nil and rawequal(U.Working, active_proxy) then
		active_views.frames = frames_view
		active_views.sweeps = sweeps_view
		return
	end
	local views = { frames = frames_view, sweeps = sweeps_view }
	local store = {}
	for k, v in pairs(U.Working) do
		if k ~= "frames" and k ~= "sweeps" then
			store[k] = v
		end
	end
	local proxy = setmetatable({}, {
		__metatable = false,
		__index = function(_, k)
			if k == "frames" or k == "sweeps" then
				return views[k]
			end
			return store[k]
		end,
		__newindex = function(_, k, v)
			if k == "frames" or k == "sweeps" then
				error("mare.ui.Working." .. k .. " is a read-only view of ui.working configuration", 2)
			end
			store[k] = v
		end,
		__pairs = function()
			local view_keys = { "frames", "sweeps" }
			return function(_, k)
				if k == nil then
					local nk, nv = next(store)
					if nk ~= nil then
						return nk, nv
					end
					return view_keys[1], views[view_keys[1]]
				end
				for idx, vk in ipairs(view_keys) do
					if k == vk then
						local nxt = view_keys[idx + 1]
						if nxt then
							return nxt, views[nxt]
						end
						return nil
					end
				end
				local nk, nv = next(store, k)
				if nk ~= nil then
					return nk, nv
				end
				return view_keys[1], views[view_keys[1]]
			end,
				nil,
				nil
		end,
	})
	active_proxy = proxy
	active_views = views
	U.Working = proxy
end

local cached_throttle_ms = nil
local cached_throttle_working = nil
local cached_throttle_leaf = nil

local function extract_throttle_ms(L)
	local ms = L and L.text_throttle_ms or nil
	if type(ms) == "number" and ms >= 0 then
		return ms
	end
	return 250
end

local function update_throttle_cache(o, L)
	local raw_ui = raw_ui_section(o)
	local raw_w = raw_ui and raw_ui.working or nil
	cached_throttle_working = raw_w
	cached_throttle_leaf = type(raw_w) == "table" and raw_w.text_throttle_ms or nil
	cached_throttle_ms = extract_throttle_ms(L)
end

function M.apply(o)
	local U = mare.ui
	local L = M.resolve(o)
	update_throttle_cache(o, L)
	local glyph_map = L.glyphs or data.frames
	install_working_views(glyph_map)
	local raw_ui = raw_ui_section(o)
	local raw_tc = raw_ui and type(raw_ui.tool_call) == "table" and raw_ui.tool_call or nil
	local tc_explicit = raw_tc ~= nil and raw_tc.spinner ~= nil
	local tc_spinner = raw_tc and raw_tc.spinner
	if tc_spinner == false or tc_spinner == "none" then
		U.ToolCall.still = true
		U.ToolCall.spinner = nil
	elseif tc_explicit then
		U.ToolCall.still = false
		if type(tc_spinner) == "string" and tc_spinner ~= "braille" then
			U.ToolCall.spinner = glyph_map[tc_spinner]
		elseif type(tc_spinner) == "table" then
			U.ToolCall.spinner = tc_spinner
		else
			U.ToolCall.spinner = nil
		end
	else
		U.ToolCall.still = L.tools == "still"
		U.ToolCall.spinner = nil
	end
	U.Working.render = function(_, info)
		return draw.row(L, data, info)
	end
end

function M.text_throttle_ms(o)
	if o == nil and cached_throttle_ms ~= nil then
		local raw_ui = type(mare.config) == "table" and type(mare.config.ui) == "table" and mare.config.ui or nil
		local raw_w = raw_ui and raw_ui.working or nil
		if rawequal(raw_w, cached_throttle_working) then
			local raw_leaf = type(raw_w) == "table" and raw_w.text_throttle_ms or nil
			if raw_leaf == cached_throttle_leaf then
				return cached_throttle_ms
			end
		end
	end
	local target = o or mare.o or mare.config or {}
	local L = M.resolve(target)
	if o == nil then
		update_throttle_cache(target, L)
		return cached_throttle_ms
	end
	return extract_throttle_ms(L)
end

--- the label for a phase ("reasoning" | "responding" | "tool" | "shell" | "exploring"): the
--- configured verb, else `default`
function M.verb(phase, default)
	local o = mare.o or mare.config or {}
	local w = type(o.ui) == "table" and o.ui.working or nil
	local v = type(w) == "table" and w.verbs
	local s = type(v) == "table" and v[phase]
	return type(s) == "string" and s ~= "" and s or default
end

return M
