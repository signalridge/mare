-- session.picker_rows: the rows of the /resume picker — the session list sorted and grouped
-- (day headings, forks under their parent), each row's select item and its preview pane.
-- Part of the `session` plugin's browser (session/browser.lua); pure building, no I/O but the
-- lazily read conversation tail of the highlighted row.
return function(deps)
	local msg_text, own_prompt = deps.msg_text, deps.own_prompt
	local M = {}

	--- "just now", "5m ago", "3h ago", "2d ago", else a date
	local function relative_time(ms)
		local d = (mare.task.now() - ms) / 1000
		if d < 60 then
			return "just now"
		elseif d < 3600 then
			return ("%dm ago"):format(d // 60)
		elseif d < 86400 then
			return ("%dh ago"):format(d // 3600)
		elseif d < 7 * 86400 then
			return ("%dd ago"):format(d // 86400)
		end
		return mare.date("%Y-%m-%d", ms)
	end

	--- "Today" / "Yesterday" / "N days ago"
	local function day_group(ms)
		local function day(t)
			return mare.date("%Y-%m-%d", t)
		end
		local now = mare.task.now()
		if day(ms) == day(now) then
			return "Today"
		elseif day(ms) == day(now - 86400000) then
			return "Yesterday"
		end
		local d = math.max(2, math.floor((now - ms) / 86400000))
		return d < 30 and (d .. " days ago") or mare.date("%Y-%m", ms)
	end

	-- preview pane for a session row: the index fields at
	-- once, the end of the conversation read only for the row the user is on (cached)
	local tail_cache = {}
	local function session_preview(s, with_tail, short)
		local meta = function(k, v)
			return ui.Line({ ui.Span(("%-9s"):format(k)):hl("MareDim"), ui.Span(tostring(v or "")) })
		end
		local items = {
			ui.Line({ ui.Span(s.title or s.preview or "(empty)"):hl("MareTitle") }),
			ui.Gap(1),
			meta("updated", relative_time(s.updated_at_ms or 0)),
			meta("messages", s.messages or "?"),
			meta("id", (short or string.sub(s.session_id or "?", 1, 8)) .. "  " .. (s.session_id or "")),
		}
		if s.cwd then
			items[#items + 1] = meta("cwd", s.cwd)
		end
		if s.labels then
			items[#items + 1] = meta("labels", table.concat(s.labels, ", "))
		end
		local tail = with_tail and tail_cache[s.path]
		if with_tail and tail == nil then
			local ok, entries = pcall(mare.store.read_tail, s.path, 40)
			tail = {}
			for _, e in ipairs(ok and entries or {}) do
				local m = e.type == "message" and e.message
				if m and (own_prompt(m) or m.role == "assistant") then
					local t = msg_text(m)
					if t ~= "" then
						tail[#tail + 1] = { role = m.role, text = t }
					end
				end
			end
			while #tail > 6 do
				table.remove(tail, 1)
			end
			tail_cache[s.path] = tail
		end
		if tail and #tail > 0 then
			items[#items + 1] = ui.Gap(1)
			for _, t in ipairs(tail) do
				local who = t.role == "user" and "you  " or "mare "
				items[#items + 1] = ui.Line({
					ui.Span(who):hl(t.role == "user" and "MareAccent" or "MareDim"),
					ui.Span(mare.text.trunc_width((t.text:gsub("%s+", " ")), 200)):hl("MareMuted"),
				})
			end
		elseif s.preview and s.preview ~= s.title then
			items[#items + 1] = ui.Gap(1)
			items[#items + 1] = ui.Paragraph(ui.Line({ ui.Span(s.preview):hl("MareMuted") }))
		end
		return ui.Lines(items)
	end

	--- the picker's rows: sorted by the chosen time, forks under their parent (the index
	--- records it), only named sessions with `st.named`. "updated" keeps the index's order
	--- `key` only labels day groups.
	local function picker_rows(list, st)
		local key = function(e)
			return (st.sort == "created" and (e.created_at_ms or e.updated_at_ms) or e.updated_at_ms) or 0
		end
		local rows, by_id, kids, pos = {}, {}, {}, {}
		for _, e in ipairs(list) do
			if not st.named or e.title then
				rows[#rows + 1] = e
				pos[e] = #rows
				if e.session_id then
					by_id[e.session_id] = e
				end
			end
		end
		-- table.sort isn't stable: equal keys fall back to the index position
		table.sort(rows, function(a, b)
			if st.sort == "created" and key(a) ~= key(b) then
				return key(a) > key(b)
			end
			return pos[a] < pos[b]
		end)
		local roots = {}
		for _, e in ipairs(rows) do
			local p = e.parent_session_id and by_id[e.parent_session_id]
			if p and p ~= e then
				kids[p] = kids[p] or {}
				table.insert(kids[p], e)
			else
				roots[#roots + 1] = e
			end
		end
		local out, placed = {}, {}
		local function put(e, depth, group)
			if placed[e] then
				return
			end
			placed[e] = true
			out[#out + 1] = { s = e, depth = depth, group = group }
			for _, k in ipairs(kids[e] or {}) do
				put(k, depth + 1, group)
			end
		end
		for _, e in ipairs(roots) do
			put(e, 0, day_group(key(e)))
		end
		-- a fork cycle in a damaged index: list what's left flat
		for _, e in ipairs(rows) do
			put(e, 0, day_group(key(e)))
		end
		return out
	end

	-- /resume rows are looks — data: each look is rows of segment names (`sep` between them); a
	-- segment is `fn(s, info) -> text, hl` or `-> { {text, hl}, … }` (nil: left out; a row with
	-- nothing left is dropped). info = {current, open = peer name?, crashed, short = short id}.
	-- `session` config: `picker_style` (the default look), `picker_looks` (add or replace looks),
	-- `picker_segments` (add or replace segments). Sources:
	--   comfortable:2558-2613 (name / time ⌁ cwd branch) and Claude
	--                 Code's list as observed (title / age · branch · …): two rows, the second dim
	--   dense:2638-2750 (one row: time, conversation)
	--   cards:368-445 (title time mark / id dir / › input)
	local LOOKS = {
		comfortable = {
			{ "title", "mark" },
			{ "time", "id", "dir", "branch", sep = " · " },
		},
		dense = { { "time_col", "title", "mark", "id" } },
		cards = { { "title", "time", "mark" }, { "id", "dir", "branch" }, { "input" } },
	}
	M.LOOK_ORDER = { "comfortable", "dense", "cards" }

	local SEGMENTS = {
		-- the session's name, else its first prompt
		title = function(s)
			return mare.text.trunc_width(s.title or s.preview or "(empty)", 60), "MareText"
		end,
		time = function(s)
			return relative_time(s.updated_at_ms or 0), "MareMuted"
		end,
		-- the dense table keeps the time in a column of its own
		time_col = function(s)
			return ("%-9s"):format(relative_time(s.updated_at_ms or 0)), "MareMuted"
		end,
		-- the session open here, in another process (its peer registry entry), or one whose last
		-- run never closed it
		mark = function(_, info)
			if info.current then
				return "← current", "MareSuccess"
			elseif info.open then
				return "◌ open in " .. info.open, "MareWarning"
			elseif info.crashed then
				return "⚠ crashed", "MareError"
			end
		end,
		-- the shortest prefix (8+ digits) no other session shares: what `-r <id>` takes back
		-- (git's short hashes; a UUIDv7's first 8 digits are its creation time)
		id = function(_, info)
			return info.short, "MareDim"
		end,
		-- the directory dim, its last component brighter (snacks.nvim picker/format.lua:87-105,
		-- Telescope's path_display):74-75)
		-- where it ran, by tab (user decision 2026-11-12): in "This project" nothing for the
		-- project's own directory, else what differs — the subdirectory (`sub/dir`) or, for
		-- another root (a worktree), its name; in "All" the `~`-relative path. Shortened first
		-- when the row is too long, from the left (the end of a path says most)
		dir = function(s, info)
			if not s.cwd then
				return nil
			end
			local p
			if info.all then
				p = ui.display_path(s.cwd, 0)
			else
				local base = info.base
				if not base or s.cwd == base then
					return nil
				end
				local sep = s.cwd:sub(#base + 1, #base + 1)
				if s.cwd:sub(1, #base) == base and (sep == "/" or sep == "\\") then
					p = s.cwd:sub(#base + 2)
				else
					p = (s.project_root or s.cwd):match("([^/\\]+)[/\\]*$") or s.cwd
				end
			end
			local head, tail = p:match("^(.*[/\\])([^/\\]+)$")
			local parts = tail and { { "⌁ ", "MareDim" }, { head, "MareDim" }, { tail, "MareMuted" } }
				or { { "⌁ ", "MareDim" }, { p, "MareMuted" } }
			parts.shrink = true
			return parts
		end,
		-- the branch whole: it is what tells sessions of one directory apart (the directory gives
		-- way first)
		branch = function(s)
			return s.git_branch, "MareMuted"
		end,
		messages = function(s)
			return s.messages and (s.messages .. " msgs"), "MareDim"
		end,
		labels = function(s)
			return s.labels and table.concat(s.labels, ", "), "MareAccent"
		end,
		-- the latest prompt (the index's last_input), when the title isn't already it
		input = function(s)
			local t = s.last_input or s.preview
			if t and t ~= (s.title or s.preview) then
				return "› " .. mare.text.trunc_width((t:gsub("%s+", " ")), 76), "MareDim"
			end
		end,
	}

	--- the rows of session `s` at fork depth `depth` in `look` (its `rows`, `segments`)
	local function look_lines(s, depth, info, look)
		local segs = look.segments or {}
		local lines = {}
		for _, names in ipairs(look.rows or LOOKS.comfortable) do
			local spans, texts = {}, {}
			local shrink -- the spans of the segment that gives way when the row is too long
			for _, name in ipairs(names) do
				local f = segs[name] or SEGMENTS[name]
				local text, hl
				if f then
					local ok, t, h = pcall(f, s, info)
					text, hl = ok and t or nil, h
				end
				if type(text) == "string" and text ~= "" then
					text = { { text, hl } }
				end
				if type(text) == "table" and #text > 0 then
					if #spans > 0 then
						spans[#spans + 1] = ui.Span(names.sep or "  "):hl("MareSubtle")
						texts[#texts + 1] = names.sep or "  "
					end
					local first = #spans + 1
					for _, p in ipairs(text) do
						spans[#spans + 1] = p[2] and ui.Span(p[1]):hl(p[2]) or ui.Span(p[1])
						texts[#texts + 1] = p[1]
					end
					if text.shrink and not shrink then
						shrink = { from = first, parts = text }
					end
				end
			end
			if #spans > 0 then
				local lead = "  "
				if depth > 0 then
					lead = string.rep("  ", depth - 1) .. (#lines == 0 and "  ↳ " or "    ")
				end
				-- too long for the list: the shrinkable segment keeps its end (`…/a/b`), so what
				-- follows it (the branch) stays whole
				if shrink and look.width then
					local used = mare.text.width(lead)
					for _, t in ipairs(texts) do
						used = used + mare.text.width(t)
					end
					local over = used - look.width
					if over > 0 then
						local p = shrink.parts
						local icon, body = p[1][1], ""
						for k = 2, #p do
							body = body .. p[k][1]
						end
						local keep = math.max(1, mare.text.width(body) - over - 1)
						-- the last `keep` columns of the body
						local chars = {}
						for _, c in utf8.codes(body) do
							chars[#chars + 1] = utf8.char(c)
						end
						local tailw, k0 = 0, #chars + 1
						while k0 > 1 and tailw + mare.text.width(chars[k0 - 1]) <= keep do
							k0 = k0 - 1
							tailw = tailw + mare.text.width(chars[k0])
						end
						local cut = "…" .. table.concat(chars, "", k0)
						for k = #p, 1, -1 do
							table.remove(spans, shrink.from + k - 1)
						end
						table.insert(spans, shrink.from, ui.Span(icon):hl(p[1][2]))
						table.insert(spans, shrink.from + 1, ui.Span(cut):hl("MareMuted"))
					end
				end
				table.insert(spans, 1, ui.Span(lead))
				lines[#lines + 1] = ui.Line(spans)
			end
		end
		return lines
	end

	--- Ctrl+E: the highlighted session opened up in place: its full id, times, place and its last
	--- input
	local function expanded_lines(s, info)
		local out = {}
		local function add(k, v)
			if v and v ~= "" then
				out[#out + 1] = { k, tostring(v) }
			end
		end
		add(
			"Session",
			s.session_id .. (info.short and info.short ~= s.session_id and ("  (" .. info.short .. ")") or "")
		)
		add(
			"Created",
			s.created_at_ms
				and (relative_time(s.created_at_ms) .. " · " .. mare.date("%Y-%m-%d %H:%M", s.created_at_ms))
		)
		add(
			"Updated",
			s.updated_at_ms
				and (relative_time(s.updated_at_ms) .. " · " .. mare.date("%Y-%m-%d %H:%M", s.updated_at_ms))
		)
		add("Directory", s.cwd)
		add("Branch", s.git_branch)
		add("Messages", s.messages)
		add("Last input", s.last_input and mare.text.trunc_width((s.last_input:gsub("%s+", " ")), 100))
		local lines = {}
		for i, kv in ipairs(out) do
			lines[i] = ui.Line({
				ui.Span(i == #out and "  └ " or "  │ "):hl("MareSubtle"),
				ui.Span(("%-12s"):format(kv[1] .. ":")):hl("MareDim"),
				ui.Span(kv[2]):hl("MareMuted"),
			})
		end
		return lines
	end

	--- the look named `name` (config looks first), as {rows, segments}
	function M.look(name, cfg)
		local mine = type(cfg.picker_looks) == "table" and cfg.picker_looks or {}
		if name == "compact" then
			name = "dense" -- the earlier name of the one-row look
		end
		return mine[name] or LOOKS[name] or LOOKS.comfortable
	end

	--- git-style short ids: for each id the shortest prefix of at least `min` (8) characters
	--- that no other id in `ids` shares — sorted once, each compared with its neighbours
	--- (O(n log n)); `-r <prefix>` and `/resume <prefix>` resolve it back
	function M.short_ids(ids, min)
		min = min or 8
		local sorted = {}
		for _, id in ipairs(ids) do
			sorted[#sorted + 1] = id
		end
		table.sort(sorted)
		local function common(a, b)
			local n = 0
			while n < #a and n < #b and a:byte(n + 1) == b:byte(n + 1) do
				n = n + 1
			end
			return n
		end
		local out = {}
		for i, id in ipairs(sorted) do
			local need = min
			if i > 1 and sorted[i - 1] ~= id then
				need = math.max(need, common(sorted[i - 1], id) + 1)
			end
			if i < #sorted and sorted[i + 1] ~= id then
				need = math.max(need, common(sorted[i + 1], id) + 1)
			end
			out[id] = id:sub(1, need)
		end
		return out
	end
	local function wrap_text(text, width)
		local lines, chars, used = {}, {}, 0
		width = math.max(12, math.floor(width or 72))
		for _, cp in utf8.codes(text) do
			local c = utf8.char(cp)
			local w = mare.text.width(c)
			if used > 0 and used + w > width then
				lines[#lines + 1] = table.concat(chars)
				chars, used = {}, 0
			end
			chars[#chars + 1] = c
			used = used + w
		end
		if #chars > 0 then
			lines[#lines + 1] = table.concat(chars)
		end
		return lines
	end

	--- select items for picker rows: day headings (not selectable), sessions, and disabled
	--- unsupported-file diagnostics; a "load more" row follows the first page. `look` =
	--- {rows, segments?, wide, expanded = id?, short = {[id] = short id}, marks = {current,
	--- open = {[id] = peer name}, crashed = {[path] = true}}}. Sessions draw as their look's rows
	--- (`lines`), with previews loaded only for the highlighted session.
	local function picker_items(rows, show_cwd, limit, deleting, more, look, unsupported)
		look = look or {}
		local marks = look.marks or {}
		local items, map = {}, {}
		local function row(item, action)
			items[#items + 1] = item
			map[#items] = action
		end
		local function append_unsupported()
			for _, path in ipairs(unsupported or {}) do
				local hint = "Version unsupported — delete with /delete <path>"
				local lines = {}
				for _, line in ipairs(wrap_text(path, (look.width or 72) - 2)) do
					lines[#lines + 1] = ui.Line({ ui.Span(line):hl("MareMuted") })
				end
				for _, line in ipairs(wrap_text(hint, (look.width or 72) - 2)) do
					lines[#lines + 1] = ui.Line({ ui.Span(line):hl("MareDim") })
				end
				row({
					item_id = "unsupported:" .. path,
					label = path,
					detail = hint,
					disabled = true,
					lines = lines,
				}, { disabled = true, unsupported = path })
			end
		end
		local group = nil
		local unsupported_added = false
		for i, r in ipairs(rows) do
			if i > limit then
				row({
					label = ("▼ %d more session%s — Enter to load"):format(
						#rows - limit,
						#rows - limit == 1 and "" or "s"
					),
				}, { more = true })
				break
			end
			local s = r.s
			if r.group ~= group and r.depth == 0 then
				group = r.group
				row({ label = "── " .. group .. " ──", header = true }, { header = true })
			end
			local label = s.title or s.preview or "(empty)"
			local detail = relative_time(s.updated_at_ms or 0)
			-- typing filters on label + detail:
			-- a named session shows its first message here so both can be searched
			if s.title and s.preview and s.preview ~= s.title then
				detail = mare.text.trunc_width(s.preview, 40) .. " · " .. detail
			end
			if s.messages then
				detail = detail .. " · " .. s.messages .. " msgs"
			end
			-- the git branch the session ran on
			if s.git_branch then
				detail = detail .. " · " .. mare.text.trunc_width(s.git_branch, 24)
			end
			if s.labels then
				detail = detail .. " · " .. table.concat(s.labels, ", ")
			end
			if show_cwd and s.cwd then
				detail = detail .. " · " .. s.cwd
			end
			local info = {
				current = s.session_id ~= nil and s.session_id == marks.current,
				open = s.session_id and (marks.open or {})[s.session_id],
				crashed = (marks.crashed or {})[s.path],
				short = (look.short or {})[s.session_id or ""] or string.sub(s.session_id or "?", 1, 8),
				all = show_cwd,
				base = look.base,
			}
			local mark = SEGMENTS.mark(s, info)
			local indent = r.depth > 0 and (string.rep("  ", r.depth - 1) .. "  ↳ ") or "  "
			local item = {
				item_id = s.session_id,
				label = (deleting and "✕ " or "") .. indent .. mare.text.trunc_width(label, 62 - 2 * r.depth),
				detail = detail .. " · " .. info.short .. (mark and (" · " .. mark) or ""),
				preview = look.wide and session_preview(s, false, info.short) or nil,
			}
			item.lines = look_lines(s, r.depth, info, look)
			if look.expanded and look.expanded == s.session_id then
				for _, l in ipairs(expanded_lines(s, info)) do
					item.lines[#item.lines + 1] = l
				end
			end
			row(item, { session = s, current = info.current, open = info.open })
			if i == 1 then
				append_unsupported()
				unsupported_added = true
			end
		end
		if not unsupported_added then
			append_unsupported()
		end
		if more and more > 0 and #rows <= limit then
			row({
				label = ("▼ %d more session%s — Enter to load"):format(more, more == 1 and "" or "s"),
			}, { more = true })
		end
		return items, map
	end

	return {
		look = M.look,
		short_ids = M.short_ids,
		LOOK_ORDER = M.LOOK_ORDER,
		relative_time = relative_time,
		day_group = day_group,
		session_preview = session_preview,
		picker_rows = picker_rows,
		picker_items = picker_items,
		tail_cache = tail_cache,
	}
end
