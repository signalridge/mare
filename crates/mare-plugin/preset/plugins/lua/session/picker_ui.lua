-- Helper functions for the /resume session picker (used by session/browser.lua).
local M = {}

local PICK_KEYS = {
	{ key = "ctrl+d", label = "delete (asks first)" },
	{ key = "ctrl+r", label = "rename" },
	{ key = "ctrl+s", label = "sort by created / updated" },
	{ key = "ctrl+n", label = "named only" },
	{ key = "ctrl+a", label = "archived" },
	{ key = "ctrl+x", label = "archive (asks first)" },
	{ key = "ctrl+e", label = "expand" },
	{ key = "ctrl+t", label = "transcript" },
	{ key = "ctrl+l", label = "layout" },
	{ key = "ctrl+g", label = "agents" },
}

local function remove_fixed(opts, target)
	if not opts.fixed then
		return
	end
	for k, s in ipairs(opts.fixed) do
		if s == target then
			table.remove(opts.fixed, k)
			break
		end
	end
end

local function cycle_look_style(P)
	local order = P.R.LOOK_ORDER
	local at = 1
	for k, n in ipairs(order) do
		if n == P.view.style then
			at = k
		end
	end
	P.view.style = order[at % #order + 1]
end

--- The rows of both tabs, one page each (`limit`), and how many more the index holds.
function M.read_lists(P, st, opts)
	local lists, more, unsupported = {}, {}, {}
	if opts.fixed then
		lists[1], more[1], unsupported[1] = opts.fixed, 0, {}
	else
		for t, scope_all in ipairs({ false, true }) do
			local l, total, invalid = P.index_rows(scope_all, true, false, st.limit, true)
			lists[t], more[t], unsupported[t] = l, math.max(0, total - st.limit), invalid
		end
	end
	return lists, more, unsupported
end

--- Build the ui.select spec and row action maps for `lists`; nil when there is nothing to list.
function M.build_spec(P, st, cur, opts, lists, more, unsupported)
	local tabs, maps, rowsets = {}, {}, {}
	st.open = st.open or P.open_elsewhere()
	local look = {
		rows = P.R.look(P.view.style, P.cfg),
		wide = (ui.width() or 0) >= 84,
		width = P.list_width(),
		base = P.project_root(),
		segments = P.cfg.picker_segments,
		expanded = st.expanded,
		short = P.short_map(),
		marks = { current = P.S.id, open = st.open, crashed = P.crashed_marks() },
	}
	for t, l in ipairs(lists) do
		local rows = P.picker_rows(l, st)
		rowsets[t] = rows
		local items, map = P.picker_items(rows, opts.fixed or t == 2, st.limit, false, more[t], look, unsupported[t])
		maps[t] = map
		local sel = 1
		for i, a in pairs(map) do
			if a.session and st.sel_id and a.session.session_id == st.sel_id then
				sel = i
			end
		end
		tabs[t] = { name = t == 1 and "This project" or "All", items = items, selected = sel }
	end
	if
		#rowsets[1] == 0
		and #(unsupported[1] or {}) == 0
		and (not rowsets[2] or #rowsets[2] == 0)
		and #(unsupported[2] or {}) == 0
		and not st.named
	then
		return nil
	end
	-- Until the highlight is reported, the cursor is on the first session of the tab
	-- (the core skips the day heading): a refresh right away keeps that one.
	if not st.sel_id then
		local first
		for i, a in pairs(maps[st.tab] or {}) do
			if a.session and (not first or i < first) then
				first, st.sel_id = i, a.session.session_id
			end
		end
	end
	local spec = {
		title = opts.title or "Resume session",
		title_right = P.options_line(st),
		-- List and preview share a wide popup about evenly; the list never narrower than a
		-- row's title, time and mark plus the border options (data: `picker_split`).
		split = P.cfg.picker_split or { ratio = 0.5, min = 76 },
		-- A search that starts filled in takes letters as text (j/k/h/l move once it's gone).
		vim = not st.prefill,
		filter = st.filter,
		keys = PICK_KEYS,
		on_highlight = function(i, t)
			local a = cur.maps[t] and cur.maps[t][i]
			-- The row the user is on survives a refresh.
			if a and a.session then
				st.sel_id, st.tab = a.session.session_id, t
			end
			-- Below 84 columns the rows carry it: no preview stacked under the list.
			if not a or not a.session or (ui.width() or 0) < 84 then
				return nil
			end
			return P.session_preview(a.session, true, P.short_id(a.session.session_id))
		end,
	}
	if #tabs > 1 then
		spec.tabs, spec.tab = tabs, st.tab
	else
		spec.items, spec.selected = tabs[1].items, tabs[1].selected
	end
	return spec, maps
end

--- Show ui.select, keeping the handle of the open one (`on_open`) so a refresh can replace its
--- items in place (`sel:update`).
function M.show_picker(P, st, cur, spec, maps, refresh_open)
	cur.maps = maps
	st.in_select, st.gen = true, st.gen + 1
	spec.on_open = function(sel)
		cur.sel = sel
		-- The crash marks of the rows on screen, read after the picker shows.
		local gen, rows = st.gen, {}
		for _, l in ipairs(cur.lists or {}) do
			for _, s in ipairs(l) do
				rows[#rows + 1] = s
			end
		end
		local skip = table.copy(st.open or {})
		skip[P.S.id or ""] = true
		P.ctx:spawn(function()
			if P.scan_crashes(rows, skip) and st.gen == gen then
				refresh_open()
			end
		end)
	end
	local r = table.pack(ui.select(spec))
	cur.sel = nil
	st.in_select = false
	return table.unpack(r, 1, r.n)
end

--- Replace the open picker's items in place when the index changes underneath it.
function M.refresh_open(P, st, cur, opts)
	if not (st.in_select and cur.sel) then
		st.stale = true
		return
	end
	local gen, sel = st.gen, cur.sel
	local lists, more, unsupported = M.read_lists(P, st, opts)
	-- The user may have chosen something while the index was being read.
	if not st.in_select or st.gen ~= gen then
		st.stale = true
		return
	end
	local spec, maps = M.build_spec(P, st, cur, opts, lists, more, unsupported)
	if not spec then
		return
	end
	cur.lists = lists
	cur.maps = maps -- The indices the picker answers with refer to the new items.
	sel:update(spec.tabs and { tabs = spec.tabs } or { items = spec.items })
end

--- Handle a picker selection or key action. Returns `done, packed_result`.
function M.handle_choice(P, st, cur, opts, i, tab, key, filter)
	if not i then
		return true
	end
	st.prefill = false
	st.tab, st.filter = tab or 1, filter or ""
	local a = cur.maps[st.tab] and cur.maps[st.tab][i]
	if not a or a.header then
		return true
	end
	-- Only session rows perform a resume action.
	-- If a host does not honor `disabled`, choosing a diagnostic just reopens the picker.
	st.sel_id = a.session and a.session.session_id or st.sel_id
	if a.more then
		st.limit = st.limit + P.PAGE
	elseif key == "ctrl+s" then
		st.sort = st.sort == "updated" and "created" or "updated"
	elseif key == "ctrl+n" then
		st.named = not st.named
	elseif key == "ctrl+a" then
		P.pick_archived(st.tab == 2)
	elseif key == "ctrl+e" and a.session then
		-- Open the highlighted session up in place, or close it.
		st.expanded = st.expanded ~= a.session.session_id and a.session.session_id or nil
	elseif key == "ctrl+l" then
		cycle_look_style(P)
	elseif key == "ctrl+t" and a.session then
		-- The whole conversation, read-only.
		local title = a.session.title or a.session.preview or a.session.session_id
		return true, table.pack(P.load_transcript(a.session.path, title))
	elseif key == "ctrl+g" then
		-- The other screen (agents-ui: two screens, cross-linked).
		local commands = P.ctx:get("commands")
		if not (commands and pcall(commands.run, commands, "/agents")) then
			ui.notify("/agents needs the agents plugin", "warning")
		end
		return true
	elseif key == "ctrl+x" and a.session then
		if P.archive_session(a.session) then
			remove_fixed(opts, a.session)
		end
	elseif key == "ctrl+d" and a.session then
		if P.delete_session(a.session) then
			P.tail_cache[a.session.path] = nil
			remove_fixed(opts, a.session)
		end
	elseif key == "ctrl+r" and a.session then
		local name = ui.input({ title = "Rename session", placeholder = a.session.title or "" })
		if name and name ~= "" and P.rename_session(a.session, name) and opts.fixed then
			a.session.title = name
		end
	elseif a.current then
		ui.notify("Already in this session")
		return true
	elseif a.session then
		if P.refuse_busy("/resume") then
			return true
		end
		return true, table.pack(P.after_pick(a.session, opts))
	end
	return false
end

return M
