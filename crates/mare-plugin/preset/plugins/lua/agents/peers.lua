-- agents.peers: the Peers tab of /agents:
-- the other mare sessions on this machine from the peer registry — name, directory,
-- busy or idle (the unified status), messages held for that user's answer
-- (`plugins.status.held`) and the session open there (`plugins.session`). Read-only here; Enter
-- hands over to `/peers` (the peers plugin, mare-plugins) to message it.
return function(svc)
	local M = {}

	local STATE = {
		working = { "●", "MareAccent" },
		blocked = { "◐", "MareWarning" },
		idle = { "○", "MareDim" },
	}

	--- "42s", "3m", "2h", "5d" since `ms`
	local function since(ms)
		local s = math.max(0, math.floor((mare.task.now() - ms) / 1000))
		if s < 60 then
			return s .. "s"
		elseif s < 3600 then
			return (s // 60) .. "m"
		elseif s < 86400 then
			return (s // 3600) .. "h"
		end
		return (s // 86400) .. "d"
	end

	--- the tab's items and their actions
	function M.items(list)
		local items, map = {}, {}
		local name_w = 0
		for _, p in ipairs(list) do
			name_w = math.min(24, math.max(name_w, mare.text.width(p.name)))
		end
		for _, p in ipairs(list) do
			local state = p.status.state or "idle"
			local sym, hl = (STATE[state] or STATE.idle)[1], (STATE[state] or STATE.idle)[2]
			local word = state
			if (p.held or 0) > 0 then
				sym, hl = "!", "MareError"
				word = ("%d held"):format(p.held)
			end
			local dir = p.cwd and ui.display_path(p.cwd, 30) or ""
			local msg = type(p.status.message) == "string" and p.status.message ~= "" and p.status.message or nil
			local age = p.started_at_ms and p.started_at_ms > 0 and since(p.started_at_ms) or nil
			local detail = word .. (dir ~= "" and ("  " .. dir) or "")
			items[#items + 1] = {
				item_id = "peer/" .. tostring(p.pid),
				label = p.name,
				detail = detail .. (p.session.name and ("  " .. p.session.name) or ""),
				-- The agents view lists other sessions in the same way: the
				-- name, what it is doing in one line (its unified status message), how long it has
				-- run; the directory dim after it
				lines = {
					ui.Line({
						ui.Span(sym .. " "):hl(hl),
						ui.Span(p.name .. string.rep(" ", math.max(0, name_w - mare.text.width(p.name)))),
						ui.Span("  " .. word):hl((p.held or 0) > 0 and "MareError" or "MareMuted"),
						ui.Span(msg and (" · " .. mare.text.trunc_width(msg, 50)) or ""):hl("MareDim"),
						ui.Span(age and (" · " .. age) or ""):hl("MareSubtle"),
						ui.Span(dir ~= "" and ("  " .. dir) or ""):hl("MareDim"),
					}),
				},
			}
			map[#items] = { peer = p }
		end
		return items, map
	end

	--- the highlighted peer: where, what it does, the session open there, held messages
	function M.preview(p)
		local function meta(k, v)
			return ui.Line({ ui.Span(("%-8s"):format(k)):hl("MareDim"), ui.Span(tostring(v)) })
		end
		local out = { ui.Line({ ui.Span(p.name):hl("MareTitle") }) }
		if p.cwd then
			out[#out + 1] = meta("dir", ui.display_path(p.cwd, 60))
		end
		if p.session.session_id then
			out[#out + 1] = meta(
				"session",
				(p.session.name and (p.session.name .. " ") or "")
					.. "("
					.. (svc.session and svc.session:short_id(p.session.session_id) or string.sub(
						p.session.session_id,
						1,
						8
					))
					.. ")"
			)
		end
		local msg = p.status.message ~= "" and p.status.message or nil
		out[#out + 1] = meta("status", (p.status.state or "idle") .. (msg and (" · " .. msg) or ""))
		out[#out + 1] = meta("held", (p.held or 0) > 0 and tostring(p.held) or "—")
		out[#out + 1] = meta(
			"pid",
			tostring(p.pid) .. (p.started_at_ms and (" · since " .. mare.date("%H:%M", p.started_at_ms)) or "")
		)
		return ui.Lines(out)
	end

	--- Enter: message it through /peers (not a sub-agent: peers are other sessions)
	function M.message(p)
		if not (svc.commands and pcall(svc.commands.run, svc.commands, "/peers " .. p.name)) then
			ui.notify(("/peers (the peers plugin) messages %s"):format(p.name), "warning")
		end
	end

	return M
end
