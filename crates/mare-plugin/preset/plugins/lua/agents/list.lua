-- agents.list: the /agents screen: tabs `This session │
-- Peers` in mare's select shell (title and tabs in the border, the counts at its right end, a
-- preview pane on a wide terminal), refreshed in place while open; Enter on a sub-agent opens its
-- conversation read-only and live (session:open_transcript), Esc there comes back here and
-- Alt+←/→ steps to the previous / next sub-agent — only in a
-- view this plugin opened. Look: structured marks in mare's colours.
return function(ctx, cfg, store, peers, svc)
	local M = {}
	local views = {} -- transcript title -> member key (the views this plugin opened)

	--- "42s", "3m04s", "1h05m"
	local function dur(ms)
		local s = math.max(0, math.floor((ms or 0) / 1000))
		if s < 60 then
			return s .. "s"
		elseif s < 3600 then
			return ("%dm%02ds"):format(s // 60, s % 60)
		end
		return ("%dh%02dm"):format(s // 3600, (s % 3600) // 60)
	end

	-- state -> symbol, highlight, word
	local STATE = {
		running = { "●", "MareAccent", "running" },
		done = { "✓", "MareSuccess", "done" },
		failed = { "✗", "MareError", "failed" },
		crashed = { "⚠", "MareWarning", "never closed" },
	}

	--- the title of a member's view: unique per member, so another member is another view
	local function view_title(m)
		return ("%s%s · %s #%s"):format(m.label, m.type and (" [" .. m.type .. "]") or "", tostring(m.run_id), m.seq)
	end

	--- one member's row; `!` and its place in the approval order while it waits (mare answers in
	--- order, so the place is shown)
	local function member_item(m, depth, name_w)
		local st = STATE[m.state] or STATE.running
		local sym, hl, word = st[1], st[2], st[3]
		if m.waiting then
			sym, hl = "!", "MareError"
			word = m.waiting == 0 and "waiting for approval" or ("waiting for approval #" .. m.waiting)
		end
		-- the row: `● name [type] state · what it did last · 42s`
		local name = m.label .. (m.type and (" [" .. m.type .. "]") or "")
		local what = m.activity or (not m.path and "no transcript" or nil)
		if m.unassociated then
			what = "unassociated" .. (what and (" · " .. what) or "")
		end
		local age = m.started and dur(mare.task.now() - m.started) or nil
		local detail = word .. (what and (" · " .. what) or "") .. (age and (" · " .. age) or "")
		local pad = math.max(0, (name_w or 0) - 2 * depth - mare.text.width(name))
		local spans = {
			ui.Span(string.rep("  ", depth) .. sym .. " "):hl(hl),
			ui.Span(name .. string.rep(" ", pad)),
			ui.Span("  " .. word):hl(m.waiting and "MareError" or hl == "MareAccent" and "MareMuted" or hl),
		}
		if what then
			spans[#spans + 1] = ui.Span(" · "):hl("MareSubtle")
			spans[#spans + 1] = ui.Span(mare.text.trunc_width(what, 60)):hl("MareDim")
		end
		if age then
			spans[#spans + 1] = ui.Span(" · " .. age):hl("MareSubtle")
		end
		return { item_id = m.key, label = name, detail = detail, lines = { ui.Line(spans) } }
	end

	local function job_item(j)
		local secs = ((j.ended_at_ms or mare.task.now()) - (j.started_at_ms or mare.task.now()))
		local detail = tostring(j.status) .. " · " .. dur(secs)
		return {
			item_id = "job/" .. tostring(j.job_id),
			label = j.label or j.description or j.job_id,
			detail = detail,
			lines = {
				ui.Line({
					ui.Span("⧗ "):hl(j.status == "running" and "MareAccent" or "MareDim"),
					ui.Span(mare.text.trunc_width(j.label or j.description or j.job_id, 40)),
					ui.Span("  " .. detail):hl("MareMuted"),
				}),
			},
		}
	end

	--- the Agents tab: run → phase headings (not selectable), members, then jobs
	local function agent_items(snap)
		local items, map = {}, {}
		local function add(item, action)
			items[#items + 1] = item
			map[#items] = action
		end
		local by_run, run_order = {}, {}
		for _, m in ipairs(snap.members) do
			local r = m.run_id or ""
			if not by_run[r] then
				by_run[r] = {}
				run_order[#run_order + 1] = r
			end
			table.insert(by_run[r], m)
		end
		-- one name column for every member (aligned column with room for run indentation), up to 30
		local name_w = 0
		for _, m in ipairs(snap.members) do
			local n = m.label .. (m.type and (" [" .. m.type .. "]") or "")
			name_w = math.min(30, math.max(name_w, mare.text.width(n) + 2))
		end
		for _, r in ipairs(run_order) do
			local list = by_run[r]
			local info = snap.runs[r] or {}
			local phased = false
			for _, m in ipairs(list) do
				phased = phased or m.phase ~= nil
			end
			-- a single sub-agent call is one row; a workflow run gets its heading
			local headed = r ~= "" and (#list > 1 or phased or info.name ~= nil)
			if headed then
				local done = 0
				for _, m in ipairs(list) do
					done = done + ((m.state == "done" or m.state == "failed") and 1 or 0)
				end
				add({
					label = ("▾ %s  %d/%d"):format(info.name or r, done, info.total or #list),
					header = true,
				}, { header = true })
			end
			local phase
			for _, m in ipairs(list) do
				if m.phase and m.phase ~= phase then
					phase = m.phase
					add({ label = "  " .. phase, header = true }, { header = true })
				end
				add(member_item(m, headed and 1 or 0, name_w), { member = m })
			end
		end
		for _, j in ipairs(snap.jobs) do
			add(job_item(j), { job = j })
		end
		return items, map
	end

	--- the preview of a member: what it is, then the end of its conversation (lazily, the
	--- highlighted row only:16-19 shows the last few too)
	local function member_preview(m)
		local function meta(k, v)
			return ui.Line({ ui.Span(("%-8s"):format(k)):hl("MareDim"), ui.Span(tostring(v)) })
		end
		local st = STATE[m.state] or STATE.running
		---@type mare.Node[]
		local out = {
			ui.Line({ ui.Span(m.label):hl("MareTitle"), ui.Span("  " .. st[3]):hl(st[2]) }),
		}
		if m.task then
			out[#out + 1] = meta("task", m.task)
		end
		if m.run_id then
			out[#out + 1] = meta("run", m.run_id .. (m.phase and (" · " .. m.phase) or ""))
		end
		if m.started then
			out[#out + 1] = meta("started", mare.date("%H:%M:%S", m.started))
		end
		if m.path then
			local ok, tail = pcall(mare.store.read_tail, m.path, 12)
			local lines = {}
			for _, e in ipairs(ok and tail or {}) do
				local msg = e.type == "message" and e.message
				if msg and (msg.role == "assistant" or msg.role == "user") then
					for _, b in ipairs(type(msg.content) == "table" and msg.content or {}) do
						if b.type == "text" and b.text ~= "" then
							lines[#lines + 1] = (b.text:gsub("%s+", " "))
						elseif b.type == "tool_call" then
							lines[#lines + 1] = "● " .. tostring(b.name)
						end
					end
				end
			end
			if #lines > 0 then
				out[#out + 1] = ui.Gap(1)
				for k = math.max(1, #lines - 4), #lines do
					out[#out + 1] = ui.Line({ ui.Span(mare.text.trunc_width(lines[k], 200)):hl("MareMuted") })
				end
			end
		end
		return ui.Lines(out)
	end

	--- the select spec for a snapshot, plus the row actions of each tab
	local function build(snap, peer_list, st)
		local a_items, a_map = agent_items(snap)
		local p_items, p_map = peers.items(peer_list)
		local maps = { a_map, p_map }
		local tabs = {
			{ name = "This session", items = a_items, selected = 1 },
			{ name = "Peers", items = p_items, selected = 1 },
		}
		for t, map in ipairs(maps) do
			for i, a in pairs(map) do
				local k = a.member and a.member.key or (a.peer and ("peer/" .. a.peer.pid))
				if k and k == st.focus then
					tabs[t].selected = i
				end
			end
		end
		local held = 0
		for _, p in ipairs(peer_list) do
			held = held + (p.held or 0)
		end
		-- header counts aligned with SPEC (`N agents · M waiting · K sessions · J held`)
		local parts = {
			("%d agent%s"):format(#snap.members, #snap.members == 1 and "" or "s"),
		}
		if snap.waiting > 0 then
			parts[#parts + 1] = ("%d waiting"):format(snap.waiting)
		end
		parts[#parts + 1] = ("%d session%s"):format(#peer_list, #peer_list == 1 and "" or "s")
		if held > 0 then
			parts[#parts + 1] = ("%d held"):format(held)
		end
		local right = table.concat(parts, " · ")
		return {
			title = "Agents",
			title_right = right,
			split = cfg.split or { ratio = 0.5, min = 80 },
			tabs = tabs,
			tab = st.tab,
			keys = { { key = "ctrl+g", label = "sessions (/resume)" } },
		},
			maps
	end

	--- whether there is anything to list (the ← key declines otherwise)
	function M.any()
		local snap = store.snapshot()
		return #snap.members > 0 or #snap.jobs > 0 or #store.peers() > 0
	end

	-- the view's header names its own keys by their current bindings (a rebind changes it)
	local VIEW_KEYS = {
		{ action = "agents.back", label = "list" },
		{ action = { "agents.prev", "agents.next" }, label = "agent" },
		{ action = "view.close", label = "close" },
	}

	--- open a member's conversation, read-only and live
	local function watch(m)
		local title = view_title(m)
		views[title] = m.key
		svc.session:load_transcript(m.path, title, { keys = VIEW_KEYS })
	end

	--- the /agents screen; `focus` = the member key (or "peer/<pid>") to start on
	function M.open(focus)
		local st = { focus = focus, tab = 1 }
		local snap, peer_list = store.snapshot(), store.peers()
		if #snap.members == 0 and #snap.jobs == 0 and #peer_list > 0 then
			st.tab = 2
		end
		if focus and focus:find("^peer/") then
			st.tab = 2
		end
		local spec, maps = build(snap, peer_list, st)
		local cur = { maps = maps }
		local wide = (ui.width() or 0) >= (cfg.preview_min_width or 84)
		if wide then
			spec.on_highlight = function(i, t)
				local a = cur.maps[t] and cur.maps[t][i]
				if a and a.member then
					return member_preview(a.member)
				elseif a and a.peer then
					return peers.preview(a.peer)
				end
			end
		end
		-- live while open: a new snapshot only when something changed (in place, sel:update)
		spec.on_open = function(sel)
			ctx:spawn(function()
				local last = nil
				while sel:is_open() do
					mare.task.sleep(cfg.refresh_ms or 1000)
					if not sel:is_open() then
						return
					end
					local s2, p2 = store.snapshot(), store.peers()
					local n_spec, n_maps = build(s2, p2, st)
					local parts = { n_spec.title_right }
					for _, tb in ipairs(n_spec.tabs) do
						for _, it in ipairs(tb.items) do
							parts[#parts + 1] = (it.label or "") .. "\0" .. (it.detail or "")
						end
						parts[#parts + 1] = "\1"
					end
					local fp = table.concat(parts, "\n")
					if fp ~= last then
						last = fp
						cur.maps = n_maps
						sel:update({ tabs = n_spec.tabs })
					end
				end
			end)
		end
		local i, tab, key = ui.select(spec)
		if not i then
			return
		end
		local a = cur.maps[tab or 1] and cur.maps[tab or 1][i]
		if key == "ctrl+g" then
			if not (svc.commands and pcall(svc.commands.run, svc.commands, "/resume")) then
				ui.notify("/resume needs the session plugin", "warning")
			end
			return
		end
		if not a or a.header then
			return
		end
		if a.member then
			if a.member.path and svc.session then
				return watch(a.member)
			end
			ui.notify(("%s left no transcript (its runner doesn't record one)"):format(a.member.label))
		elseif a.job then
			if not (svc.commands and pcall(svc.commands.run, svc.commands, "/jobs")) then
				ui.notify("/jobs needs the jobs plugin", "warning")
			end
		elseif a.peer then
			peers.message(a.peer)
		end
	end

	--- the member of the view on screen, when this plugin opened it
	local function current_view()
		local t = ui.transcript_title()
		return t and views[t] and t or nil
	end

	--- Esc in a member's view: back to the list, on that member; elsewhere declines
	function M.back()
		local t = current_view()
		if not t then
			return false
		end
		ui.transcript_close(t)
		M.open(views[t])
	end

	--- Alt+←/→ in a member's view: the previous / next member with a conversation (wrapping);
	--- elsewhere declines
	function M.step(delta)
		local t = current_view()
		if not t then
			return false
		end
		local list = {}
		for _, m in ipairs(store.snapshot().members) do
			if m.path then
				list[#list + 1] = m
			end
		end
		local at
		for k, m in ipairs(list) do
			if m.key == views[t] then
				at = k
			end
		end
		if not at or #list < 2 then
			return true
		end
		watch(list[(at - 1 + delta) % #list + 1])
	end

	return M
end
