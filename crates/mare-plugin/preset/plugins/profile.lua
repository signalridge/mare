-- profile: /profile — startup phases and per-plugin load times (lazy.nvim
-- `:Lazy profile` / Neovim `--startuptime` style).
return {
	name = "profile",
	-- `bus` for the mare event; commands is followed below
	inject = { "bus" },
	apply = function(ctx)
		-- tool-table changes between model requests (a changed tool list invalidates the
		-- provider's prompt cache from that point): recorded here, shown in /profile
		local tool_changes, last_tools = {}, nil -- newest last, at most 50
		-- request/build is a bus waterfall (`function(req, ev)`): let later listeners run first via `ev.next()`
		ctx:on("request/build", function(req, ev)
			req = ev.next()
			local names, defs = {}, {}
			for _, t in ipairs((type(req) == "table" and req.tools) or {}) do
				names[#names + 1] = t.name
				defs[t.name] = mare.crypto.sha256(mare.json.encode(t))
			end
			local cur = { names = names, defs = defs }
			if last_tools then
				local added, removed, changed = {}, {}, {}
				for _, n in ipairs(names) do
					if not last_tools.defs[n] then
						added[#added + 1] = n
					elseif last_tools.defs[n] ~= defs[n] then
						changed[#changed + 1] = n
					end
				end
				for _, n in ipairs(last_tools.names) do
					if not defs[n] then
						removed[#removed + 1] = n
					end
				end
				local reordered = #added == 0
					and #removed == 0
					and table.concat(names, ",") ~= table.concat(last_tools.names, ",")
				if #added + #removed + #changed > 0 or reordered then
					local rec = {
						at_ms = mare.task.now(),
						added = added,
						removed = removed,
						changed = changed,
						reordered = reordered or nil,
					}
					tool_changes[#tool_changes + 1] = rec
					if #tool_changes > 50 then
						table.remove(tool_changes, 1)
					end
					ctx.bus:emit("profile/tools-changed", rec)
				end
			end
			last_tools = cur
			return req
		end)
		-- registered while the optional `commands` service is up (a child fiber)
		ctx:inject({ "commands" }, function(sub)
			sub.commands:register("profile", {
				description = "Startup timeline and plugin load times",
				run = function()
					local rows = mare.profile.report()
					---@type mare.Node[]
					local lines = {
						ui.Line({ ui.Span(("%9s %8s  %s"):format("clock", "self", "step")):hl("MareDim") }),
					}
					local plugins_total = 0
					local yielded_plugins = {}
					for _, r in ipairs(rows) do
						local is_plugin = string.find(r.label, "^plugin ") ~= nil
						local pname, porigin, pflags = string.match(r.label, "^plugin ([^%s]+) %(([^%)]+)%)%s*(.*)$")
						local plugin_yielded = porigin ~= nil
							and pflags ~= nil
							and string.find(pflags, "yielded", 1, true) ~= nil
						if plugin_yielded then
							yielded_plugins[#yielded_plugins + 1] = {
								name = pname,
								origin = porigin,
								took = r.took,
							}
						end
						if is_plugin then
							plugins_total = plugins_total + r.took
						end
						local slow = (is_plugin and r.took > 5) or (not is_plugin and r.took > 30)
						lines[#lines + 1] = ui.Line({
							ui.Span(("%8.2fms "):format(r.at)):hl("MareMuted"),
							ui.Span(r.took > 0 and ("%7.2fms  "):format(r.took) or ("%9s  "):format(""))
								:hl(slow and "MareWarning" or "MareDim"),
							ui.Span(r.label)
								:hl(plugin_yielded and "MareWarning" or (is_plugin and "MareText" or "MareAccent")),
						})
					end
					lines[#lines + 1] = ui.Gap()
					lines[#lines + 1] = ui.Line({
						ui.Span(
							("plugins: %.2fms total · budgets: startup ≤ 30ms, plugin ≤ 5ms"):format(plugins_total)
						)
							:hl("MareHint"),
					})
					if #yielded_plugins > 0 then
						lines[#lines + 1] = ui.Gap()
						lines[#lines + 1] = ui.Line({
							ui.Span(("warning: %d plugin(s) yielded during apply:"):format(#yielded_plugins))
								:hl("MareWarning"),
						})
						for _, y in ipairs(yielded_plugins) do
							lines[#lines + 1] = ui.Line({
								ui.Span(
									("  %s (%s) — defer async work out of apply (e.g. ctx:spawn or ui/ready)"):format(
										y.name,
										y.origin
									)
								):hl("MareWarning"),
							})
						end
					end
					-- frames over 16ms, newest first
					local slow = ui.slow_frames and ui.slow_frames() or {}
					if #slow > 0 then
						lines[#lines + 1] = ui.Gap()
						lines[#lines + 1] =
							ui.Line({ ui.Span(("slow frames (> 16ms): %d"):format(#slow)):hl("MareAccent") })
						for i = #slow, math.max(1, #slow - 19), -1 do
							local f = slow[i]
							lines[#lines + 1] = ui.Line({
								ui.Span(("%9.1fs %6.1fms  "):format(f.at_ms / 1000, f.ms)):hl("MareMuted"),
								ui.Span(f.what):hl("MareText"),
							})
						end
					end
					local o = mare and (mare.o or mare.config) or {}
					local slow_ms = tonumber(o.slow_callback_ms) or 1000
					local aggregate_ms = math.min(slow_ms, 2)
					local record_ms = tonumber(o.slow_callback_record_ms) or 1000
					local table_rows = tonumber(o.slow_callback_table_rows) or 100
					-- Lua callbacks over min(slow_callback_ms, 2ms), aggregated by (plugin, event)
					local timings = mare.profile.callback_timings(table_rows) or {}
					if #timings > 0 then
						lines[#lines + 1] = ui.Gap()
						lines[#lines + 1] = ui.Line({
							ui.Span(("callback timing (> %gms): %d"):format(aggregate_ms, #timings)):hl("MareAccent"),
						})
						lines[#lines + 1] = ui.Line({
							ui.Span(("%7s %10s %10s  %s"):format("count", "total", "max", "plugin / event"))
								:hl("MareDim"),
						})
						for _, row in ipairs(timings) do
							lines[#lines + 1] = ui.Line({
								ui.Span(("%7d %8.1fms %8.1fms  "):format(row.count, row.total_ms, row.max_ms))
									:hl("MareMuted"),
								ui.Span(("[%s] %s"):format(row.plugin, row.event)):hl("MareText"),
							})
						end
					end
					-- Lua callbacks over slow_callback_record_ms, newest first
					local slow_cbs = mare.profile.slow_callbacks() or {}
					if #slow_cbs > 0 then
						lines[#lines + 1] = ui.Gap()
						lines[#lines + 1] = ui.Line({
							ui.Span(("slow callbacks (> %gms): %d"):format(record_ms, #slow_cbs)):hl("MareAccent"),
						})
						for i = #slow_cbs, math.max(1, #slow_cbs - 19), -1 do
							local cb = slow_cbs[i]
							lines[#lines + 1] = ui.Line({
								ui.Span(("%9.1fs %6.1fms  "):format(cb.at_ms / 1000, cb.ms)):hl("MareMuted"),
								ui.Span(("[%s] %s"):format(cb.plugin, cb.event)):hl("MareText"),
							})
						end
					end
					if #tool_changes > 0 then
						lines[#lines + 1] = ui.Gap()
						lines[#lines + 1] = ui.Line({
							ui.Span(("tool list changes (each one breaks the prompt cache): %d"):format(#tool_changes))
								:hl("MareAccent"),
						})
						for i = #tool_changes, math.max(1, #tool_changes - 19), -1 do
							local c = tool_changes[i]
							local parts = {}
							if #c.added > 0 then
								parts[#parts + 1] = "+" .. table.concat(c.added, " +")
							end
							if #c.removed > 0 then
								parts[#parts + 1] = "-" .. table.concat(c.removed, " -")
							end
							if #c.changed > 0 then
								parts[#parts + 1] = "~" .. table.concat(c.changed, " ~")
							end
							if c.reordered then
								parts[#parts + 1] = "reordered"
							end
							lines[#lines + 1] = ui.Line({
								ui.Span(mare.date("%H:%M:%S  ", c.at_ms)):hl("MareMuted"),
								ui.Span(table.concat(parts, "  ")):hl("MareText"),
							})
						end
					end
					-- model parameters adjusted to what a model offers (llm/params: clamped, dropped,
					-- passed as is) — never silent
					local okl, L = pcall(ctx.get, ctx, "llm")
					local adj = okl and L and L.params_log and L:params_log() or {}
					if #adj > 0 then
						lines[#lines + 1] = ui.Gap()
						lines[#lines + 1] =
							ui.Line({ ui.Span(("model parameter adjustments: %d"):format(#adj)):hl("MareAccent") })
						for i = #adj, math.max(1, #adj - 19), -1 do
							local r = adj[i]
							lines[#lines + 1] = ui.Line({
								ui.Span(mare.date("%H:%M:%S  ", r.at_ms)):hl("MareMuted"),
								ui.Span(
									("%s %s %s→%s (%s)"):format(
										tostring(r.model),
										tostring(r.param),
										tostring(r.from),
										tostring(r.to),
										tostring(r.kind)
									)
								):hl("MareText"),
							})
						end
					end
					ui.overlay("Profile", ui.Lines(lines))
				end,
			})
		end)
	end,
}
