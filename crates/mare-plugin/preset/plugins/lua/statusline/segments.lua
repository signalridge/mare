-- statusline.segments: one function per segment id → { full = pieces, short = pieces? } or nil
-- (not shown). A piece is { text, style }. `P` is the statusline plugin's probe (its live
-- state), `L` the look (statusline.looks). The llm data comes through the llm service's own API
-- (`current()`, `effort()`); usage through the usage service.
-- Cold helper modules (`statusline.hints`, `statusline.compact`, `statusline.tips`) are loaded on demand.
return function(ctx, P, L, label_cache_box)
	local ui = mare.ui
	local S = {}

	local function one(text, style)
		return { full = { { text, style } } }
	end

	-- Key display labels (`key -> key_label`) are populated lazily from `ctx.ui:get_keymap("editor")`
	-- on cache miss so statusline redraws (such as `llm/providers-changed` during startup) do not
	-- snapshot and schema-validate the entire keymap on every render. Only labels actually present
	-- in the snapshot are cached (never negatively caching raw fallback keys), and the cache is
	-- cleared on `ui/keymap-changed` and `reload/completed`.
	local cache_box = label_cache_box
	if not cache_box then
		cache_box = { map = {} }
		local function clear_label_cache()
			cache_box.map = {}
		end
		ctx:on("ui/keymap-changed", clear_label_cache)
		ctx:on("reload/completed", clear_label_cache)
	end
	local labels_view = setmetatable({}, {
		__index = function(_, key)
			local cached = cache_box.map[key]
			if cached ~= nil then
				return cached
			end
			local snapshot = ctx.ui:get_keymap("editor")
			for _, binding in ipairs(snapshot.bindings or {}) do
				if binding.key ~= nil and binding.key_label ~= nil then
					cache_box.map[binding.key] = binding.key_label
				end
			end
			return cache_box.map[key] or key
		end,
	})

	S.hints = function()
		if not L.hints then
			return nil
		end
		local list = require("statusline.hints").list(P.activity(), L, labels_view)
		if #list == 0 then
			return nil
		end
		local ascii = ui.icon_set and ui.icon_set() == "ascii"
		local function pieces(n)
			local out = {}
			for i = 1, math.min(n, #list) do
				if i > 1 then
					out[#out + 1] = { ascii and " - " or " · ", "MareSubtle" }
				end
				out[#out + 1] = { list[i], "MareDim" }
			end
			return out
		end
		return { full = pieces(#list), short = pieces(1) }
	end

	S.flash = function()
		local f = P.flash()
		return f and one(f, "MareAccent") or nil
	end

	S.tips = function()
		if not P.shown("tips") then
			return nil
		end
		local t = require("statusline.tips").current(L.tips)
		return t and one(t, "MareDim") or nil
	end

	S.mode = function()
		if L.mode == "off" or not P.shown("mode") then
			return nil
		end
		local mode, partial = P.mode()
		if not mode then
			return nil
		end
		local text = partial and "PARTIAL" or (P.MODE_TEXT[mode] or string.upper(mode))
		local style = partial and "MareModeCustom" or (P.MODE_STYLE[mode] or "MareModeCustom")
		if L.mode == "capsule" then
			-- mare's capsule: Powerline half-circle caps with a Nerd Font, else
			-- padding on the background (the classic bar's ui.Segments rule)
			local bg = (ui.style(style) or {}).bg
			if bg and ui.icon_set and ui.icon_set() == "nerd" then
				return {
					full = {
						{ ui.icon("sep_left"), nil, false, bg },
						{ text, style },
						{ ui.icon("sep_right"), nil, false, bg },
					},
				}
			end
			return { full = { { " " .. text .. " ", style } } }
		end
		-- a word in the capsule's colour; lower case reads quieter
		local fg = ({
			MareModeReadOnly = "MareSuccess",
			MareModeAuto = "MareInfo",
			MareModeFull = "MareError",
			MareModeCustom = "MareWarning",
		})[style] or "MareWarning"
		return { full = { { string.lower(text), L.mode == "dim" and "MareDim" or fg, true } } }
	end

	-- warnings that never give way: --no-session, read-only view, startup failures
	S.flags = function()
		local out = {}
		local function add(t)
			if #out > 0 then
				out[#out + 1] = { " ", nil }
			end
			out[#out + 1] = { t, "MareWarning" }
		end
		if P.ephemeral() then
			add("ephemeral")
		end
		if P.readonly() then
			add("read-only")
		end
		local failed = mare.plugins.failures()
		if #failed > 0 then
			local text = #failed > 1 and (#failed .. " startup errors")
				or failed[1].kind == "config" and "config error"
				or ("plugin " .. failed[1].plugin .. " failed")
			add(text .. (ui.icon_set and ui.icon_set() == "ascii" and " - /reload" or " · /reload"))
		end
		return #out > 0 and { full = out } or nil
	end

	S.mcp = function()
		local n = P.mcp_connecting()
		local ellipsis = (ui.icon_set and ui.icon_set() == "ascii") and "..." or "…"
		return n > 0
				and {
					full = { { "MCP connecting " .. n, L.model == "muted" and "MareStatus" or "MareDim" } },
					short = { { "MCP " .. n .. ellipsis, "MareDim" } },
				}
			or nil
	end

	S.workflow = function()
		return P.workflow() and one(ui.icon("workflow") .. " workflow", "MareAccent") or nil
	end

	S.peer_allow = function()
		local a = P.peer_allow and P.peer_allow()
		return a and one("peer: " .. table.concat(a, ","), "MareWarning") or nil
	end

	S.jobs = function()
		local n = P.jobs()
		if n == 0 then
			return nil
		end
		if L.jobs == "bracket" then
			return {
				full = { { ("[%d %s running]"):format(n, n == 1 and "job" or "jobs"), "MareAccent" } },
				short = { { "[" .. n .. "]", "MareAccent" } },
			}
		end
		return one(ui.icon("jobs") .. " " .. n, "MareStatus")
	end

	S.model = function()
		local m = ctx.llm:current()
		if not m then
			return one(
				(ui.icon_set and ui.icon_set() == "ascii") and "no model - /login" or "no model · /login",
				"MareWarning"
			)
		end
		local name = P.shown("model") and (m.name or m.id) or nil
		local e = P.shown("effort") and m.reasoning and L.effort ~= "off" and ctx.llm:effort() or nil
		local style = ({ dim = "MareDim", muted = "MareMuted" })[L.model] or "MareText"
		local full = {}
		if name then
			full[#full + 1] = { name, style }
		end
		if e then
			local t = L.effort == "reasoning" and ("thinking: " .. e) or e
			full[#full + 1] = { (name and " " or "") .. t, L.model == "dim" and "MareDim" or "MareMuted" }
		end
		if #full == 0 then
			return nil
		end
		return { full = full, short = name and { { name, style } } or full }
	end

	S.cwd = function()
		if not P.shown("cwd") then
			return nil
		end
		if L.cwd_mode == "display_path" then
			if ui.width() < (P.cwd_min_terminal_width or 100) then
				return nil
			end
			return one(ui.display_path(mare.cwd, P.cwd_width or 24), "MareDim")
		end
		local p = ui.display_path(mare.cwd, 0)
		local sep = string.find(p, "\\", 1, true) and "\\" or "/"
		local parts = {}
		for seg in string.gmatch(p, "[^/\\]+") do
			parts[#parts + 1] = seg
		end
		local n = L.cwd_segments or 3
		if #parts > n then
			local ellipsis = (ui.icon_set and ui.icon_set() == "ascii") and "..." or "…"
			p = ellipsis .. sep .. table.concat(parts, sep, #parts - n + 1)
		end
		return one(p, "MareDim")
	end

	S.branch = function()
		local b = P.branch()
		if not b or not P.shown("branch") then
			return nil
		end
		return one(ui.icon("branch") .. " " .. b, "MareDim")
	end

	S.pr = function()
		local pr = P.pr()
		if not pr or not P.shown("pr") then
			return nil
		end
		return { full = { { ui.icon("pr") .. " #" .. tostring(pr.number), "MareAccent", false, nil, pr.url } } }
	end

	S.changes = function()
		local n = P.changed()
		if not n or n == 0 or not P.shown("changes") then
			return nil
		end
		return one(ui.icon("changes") .. " " .. n, "MareDim")
	end

	S.cost = function()
		local usage_service = ctx:get("usage")
		local t = usage_service and usage_service:totals()
		if not t or (t.cost or 0) <= 0 or not P.shown("cost") then
			return nil
		end
		return one(("$%.2f"):format(t.cost), "MareDim")
	end

	S.quota = function()
		local text, low = P.quota()
		if not text or not P.shown("quota") then
			return nil
		end
		return one(text, low and "MareWarning" or "MareDim")
	end

	S.context = function()
		local m = ctx.llm:current()
		local usage_service = ctx:get("usage")
		local used = usage_service and usage_service:context_tokens() or 0
		if not (m and m.context_window) or not P.shown("context") then
			return nil
		end
		local frac = used / m.context_window
		local pct = math.floor(frac * 100 + 0.5)
		local style = L.model == "dim" and "MareDim" or "MareMuted"
		if L.context_color then
			style = pct >= 90 and "MareError" or (pct >= 80 and "MareWarning" or (pct >= 60 and "MareInfo" or style))
		end
		-- within 10% of the auto-compaction threshold: say so, in the warning colour
		local soon = false
		if used > 0 then
			local limit = require("statusline.compact")(ctx:get("compact"), m)
			soon = limit and used > limit * 0.9 or false
		end
		local warning = style == "MareError" and "MareError" or "MareWarning"
		local soon_msg = (ui.icon_set and ui.icon_set() == "ascii") and " - compacts soon" or " · compacts soon"
		local tail = soon and { { soon_msg, warning } } or {}
		local function with(text)
			local out = { { text, soon and warning or style } }
			for _, t in ipairs(tail) do
				out[#out + 1] = t
			end
			return out
		end
		if L.context == "left" then
			return { full = with(("%d%% context left"):format(math.max(0, 100 - pct))) }
		elseif L.context == "gauge" then
			local gw = P.gauge_width or 6
			return {
				full = with(
					("%s %s/%s %d%%"):format(ui.Gauge(frac, gw), P.fmt_k(used), P.fmt_k(m.context_window), pct)
				),
				short = with(("%s %d%%"):format(ui.Gauge(frac, gw), pct)),
			}
		end
		return {
			full = with(("context: %d%% (%s/%s)"):format(pct, P.fmt_k(used), P.fmt_k(m.context_window))),
			short = with(("context: %d%%"):format(pct)),
		}
	end

	return S
end
