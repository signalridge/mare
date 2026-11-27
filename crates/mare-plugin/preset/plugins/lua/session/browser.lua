-- Session index, resume picker and search. Constructed on first use after the entry has
-- registered its services and commands. The session state remains owned by the entry.
return function(deps)
	local ctx, S, dir = deps.ctx, deps.state, deps.dir
	local msg_text, own_prompt, after_pick = deps.msg_text, deps.own_prompt, deps.after_pick
	local delete_files, annotate = deps.delete_files, deps.annotate
	local refresh = deps.refresh
	local cfg = deps.cfg or {}
	local R = require("session.picker_rows")({ msg_text = msg_text, own_prompt = own_prompt })
	local relative_time, session_preview = R.relative_time, R.session_preview
	local short_id -- (below) the git-style short id of a session
	local picker_rows, picker_items, tail_cache = R.picker_rows, R.picker_items, R.tail_cache
	local project_root -- initialized below; also used to scope unsupported paths to this project

	local function unsupported_in_scope(paths, all, project)
		if all then
			return paths
		end
		local cwd = project and project_root() or dir
		local session_dir = mare.store.dir(cwd)
		local sessions_root = session_dir:match("^(.*)[/\\][^/\\]+$")
		local root_slug = session_dir:match("([^/\\]+)$")
		if not sessions_root or not root_slug then
			return {}
		end
		local sep = session_dir:find("\\", 1, true) and "\\" or "/"
		local prefix = sessions_root .. sep
		local project_prefix = root_slug:sub(1, -3) .. "-"
		local out = {}
		for _, path in ipairs(paths or {}) do
			if path:sub(1, #prefix) == prefix then
				local slug = path:sub(#prefix + 1):match("^([^/\\]+)")
				if slug and (slug == root_slug or (project and slug:sub(1, #project_prefix) == project_prefix)) then
					out[#out + 1] = path
				end
			end
		end
		return out
	end

	--- index rows (newest first) of this directory (`project`: of the project root — the
	--- repository root Git discovers (nearest ancestor with a valid Git marker), else the
	--- directory itself; lists the sessions
	--- of the repository's other worktrees too, CHANGELOG v2.1.94 "resume sessions from other
	--- worktrees of the same repo"), or of all with `all`: sessions and forks with messages
	--- (`any`: empty and archived ones too — resolving an id or a name), one page
	--- of `limit` rows (nil: all). A row whose file is gone (deleted behind the index's back,
	--- until the next reconcile) is skipped, never resumed as a new empty session. Returns the
	--- rows and how many the index holds in all.
	local function index_rows(all, include_current, any, limit, project)
		local rows, total, unsupported = mare.store.index_read({
			scope = all and "all" or (project and "project" or "cwd"),
			archived = any and "any" or false,
			nonempty = not any,
			limit = limit,
		})
		refresh.warn_unsupported(unsupported)
		local out = {}
		for _, e in ipairs(rows) do
			if (include_current or e.session_id ~= S.id) and mare.fs.exists(e.path) then
				out[#out + 1] = e
			end
		end
		local invalid = {}
		for _, path in ipairs(unsupported_in_scope(unsupported, all, project)) do
			if mare.fs.exists(path) then
				invalid[#invalid + 1] = path
			end
		end
		return out, total or #out, invalid
	end

	--- the same, after a catch-up with the session files: for lookups that need the answer now
	--- (an id, `-N`, content search` ScanAndRepair — stat only for
	--- unchanged files, a few ms, at most once per 2 s). The picker doesn't: it opens on
	--- `index_rows` as it is and refreshes when the background reconcile changed something.
	local function index_list(...)
		refresh.catch_up()
		return index_rows(...)
	end

	--- An archived session isn't resumed by id or name: say how to bring it back. The archive
	--- list of the picker (Ctrl+A) unarchives with Enter.
	local function refuse_archived(s)
		if s.archived then
			return nil,
				("session %s is archived — unarchive it first: /resume, Ctrl+A lists the archived sessions, Enter unarchives"):format(
					short_id(s.session_id)
				)
		end
		return s.path, s
	end

	--- resolve_resume(arg, all) -> path, known | nil, err. `-N`: the Nth most recent session
	--- here; a file path; otherwise an exact id, the session's name
	--- (case-insensitive) or an id prefix — several matches are an error, never "the newest"
	--- . `interactive` (the TUI's `-r <arg>`, Claude
	--- Code): only a full id, `-N` or a file resumes at once; anything else comes back as
	--- nil, nil, true — "search": the caller opens the picker with `arg` as the search text.
	local function resolve_resume(arg, all, interactive)
		if arg == nil or arg == "" then
			return nil, "no session given"
		end
		local n = tonumber(arg)
		if n and n < 0 and math.floor(n) == n then
			local list = index_list(all)
			local s = list[-n]
			if not s then
				return nil,
					("there %s only %d earlier session%s%s"):format(
						#list == 1 and "is" or "are",
						#list,
						#list == 1 and "" or "s",
						all and "" or " in this directory"
					)
			end
			return s.path, s
		end
		if (arg:find("[/\\]") or arg:find("%.jsonl$")) and mare.fs.exists(arg) then
			return mare.fs.resolve(arg)
		end
		local hits, seen = {}, {}
		local lower = arg:lower()
		for _, s in ipairs(index_list(true, true, true)) do
			if s.session_id == arg then
				return refuse_archived(s)
			end
			if interactive then
				goto next_row
			end
			local by_name = s.title and s.title:lower() == lower
			local by_id = s.session_id and s.session_id:sub(1, #arg) == arg
			if (by_name or by_id) and not seen[s.path] then
				seen[s.path] = true
				hits[#hits + 1] = s
			end
			::next_row::
		end
		if interactive then
			return nil, nil, true
		end
		if #hits == 1 then
			return refuse_archived(hits[1])
		elseif #hits > 1 then
			local names = {}
			for i = 1, math.min(5, #hits) do
				local s = hits[i]
				names[#names + 1] = ("%s (%s)"):format(
					s.title or mare.text.trunc_width(s.preview or "", 30),
					short_id(s.session_id)
				)
			end
			return nil,
				("%q matches %d sessions: %s%s — give more of the id"):format(
					arg,
					#hits,
					table.concat(names, ", "),
					#hits > 5 and ", …" or ""
				)
		end
		return nil, ("no session matches %q"):format(arg)
	end

	local function delete_session(s)
		local choice = ui.select({
			title = "Delete session?",
			body = ("Delete session %q? This cannot be undone."):format(s.title or s.preview or s.session_id),
			items = { "Cancel", "Delete session" },
			selected = 1,
			allow_search = false,
		})
		if choice ~= 2 then
			return false
		end
		local ok, err = delete_files(s.path, s.session_id)
		if not ok then
			ui.notify("Session not deleted: " .. tostring(err), "warning")
			return false
		end
		ui.notify("Deleted session " .. (short_id(s.session_id) or "?"))
		return true
	end

	local function rename_session(s, name)
		local ok, err = annotate(s.path, { type = "name", name = name })
		if not ok then
			ui.notify("Not renamed: " .. err, "warning")
		end
		return ok
	end

	local PAGE = 100

	-- the /resume look (picker_rows' looks): `picker_style` to start with, Ctrl+L steps through
	-- them for this process
	local view = { style = cfg.picker_style or "comfortable" }

	-- short ids: the shortest prefix (8+) unique among the sessions `-r` resolves (sessions and
	-- forks, archived too), read from the index again when a reconcile changed it or an id isn't
	-- known yet (a new session) — at most every 2 s
	local shorts = { map = {}, changes = -1, at = 0 }
	local function short_map()
		local now = mare.task.now()
		if shorts.changes ~= refresh.changes or (shorts.miss and now - shorts.at > 2000) then
			local ok, rows = pcall(mare.store.index_read, { scope = "all", archived = "any" })
			local ids = {}
			for _, e in ipairs(ok and rows or {}) do
				ids[#ids + 1] = e.session_id
			end
			if S.id then
				ids[#ids + 1] = S.id
			end
			shorts.map, shorts.changes, shorts.at, shorts.miss = R.short_ids(ids), refresh.changes, now, false
		end
		return shorts.map
	end
	--- the short id of session `id` (git-style shortest unique prefix, 8+ characters)
	function short_id(id)
		if type(id) ~= "string" then
			return nil
		end
		local s = short_map()[id]
		if not s then
			shorts.miss = true
			s = short_map()[id]
		end
		return s or id:sub(1, 8)
	end

	--- the sessions open in other processes: their peer registry entries (`plugins.session`,
	--- published by this plugin there), id -> the process's peer name
	local function open_elsewhere()
		local out = {}
		local ok, list = pcall(function()
			return mare.peers and mare.peers.list() or {}
		end)
		local me = mare.peers and mare.peers.self()
		for _, p in ipairs(ok and list or {}) do
			local sess = type(p.plugins) == "table" and p.plugins.session
			if type(sess) == "table" and type(sess.session_id) == "string" and not (me and me.pid == p.pid) then
				out[sess.session_id] = tostring(p.name or p.pid)
			end
		end
		return out
	end

	-- crash marks (the resume's rule
	-- `running` one), by path, for the `updated` time they were read at: the end of the file is
	-- read in the background once the picker is open, never before it shows. The last 3 entries
	-- hold `closed` after a clean exit (a rename or archive from a picker may follow it); else
	-- the last 64 are searched for the latest marker. No marker there: no mark — a file from
	-- before the markers, or a run longer than that, isn't called a crash on a guess.
	local crash_cache = {}
	local function crashed_marks()
		local out = {}
		for path, c in pairs(crash_cache) do
			if c.crashed then
				out[path] = true
			end
		end
		return out
	end
	--- read the ends of `rows` not known yet (skipping `skip` ids); true when one is a crash
	local function scan_crashes(rows, skip)
		local found = false
		for _, s in ipairs(rows) do
			local c = crash_cache[s.path]
			if s.path and not skip[s.session_id or ""] and not (c and c.updated_at_ms == s.updated_at_ms) then
				local last
				for _, n in ipairs({ 3, 64 }) do
					local ok, tail = pcall(mare.store.read_tail, s.path, n)
					for _, e in ipairs(ok and tail or {}) do
						if e.type == "state" then
							last = e.state
						end
					end
					if last == "closed" or n == 64 or not ok then
						break
					end
					last = nil
				end
				crash_cache[s.path] = { updated_at_ms = s.updated_at_ms, crashed = last == "running" }
				found = found or last == "running"
			end
		end
		return found
	end

	--- the root of this project (the repository root Git discovers, else the directory itself —
	--- the index's `project_root` rule): "This project" rows show only where they differ from it
	local root_cache
	project_root = function()
		if root_cache == nil then
			local d = mare.cwd
			root_cache = d
			while d and d ~= "" do
				local gitdir, stop = mare.fs.git_dir(d)
				if gitdir then
					root_cache = d
					break
				elseif stop then
					break -- an invalid `.git` file: no repository (the index's project_root rule)
				end
				local up = d:match("^(.*)[/\\][^/\\]+$")
				if not up or up == d then
					break
				end
				d = up ~= "" and up or nil
			end
		end
		return root_cache
	end

	--- the columns a row has in the list (the select popup's rule: 2/5 of a wide popup next to
	--- the preview by default, the picker's `split` here; less the borders and padding)
	local function list_width()
		local w = ui.width() or 80
		local split = cfg.picker_split or { ratio = 0.5, min = 76 }
		if w >= 84 then
			local pop = math.max(20, math.min(140, w - 2))
			local lw = math.max(math.floor(pop * (split.ratio or 0.4)), split.min or 24)
			return math.max(24, math.min(lw, pop - 21)) - 4
		end
		return math.max(20, math.min(96, w - 4)) - 4
	end

	--- `[updated] created · [all] named` — the sort and the filter (the current value in
	--- brackets, compact so both fit an 80-column top border), at the right end of the picker's
	--- top border
	local function options_line(st)
		local spans = {}
		local function opt(name, on)
			spans[#spans + 1] = on and ui.Span("[" .. name .. "]"):hl("MareAccent") or ui.Span(name):hl("MareDim")
		end
		opt("updated", st.sort ~= "created")
		spans[#spans + 1] = ui.Span(" ")
		opt("created", st.sort == "created")
		spans[#spans + 1] = ui.Span(" · "):hl("MareSubtle")
		opt("all", not st.named)
		spans[#spans + 1] = ui.Span(" ")
		opt("named", st.named)
		return ui.Line(spans)
	end

	--- archive a session other than this one from the picker: an archive entry in its file (the
	--- same as /archive; refused while another process has it open), then its index row
	local function archive_session(s)
		if s.session_id == S.id then
			ui.notify("This is the current session — /archive archives it and starts a new one")
			return false
		end
		if
			not ui.confirm(
				("Archive session %q? Ctrl+A lists archived sessions; Enter there brings it back."):format(
					s.title or s.preview or s.session_id
				),
				true
			)
		then
			return false
		end
		local sess = deps.session
		if not sess then
			ui.notify("Not archived: session service unavailable", "warning")
			return false
		end
		local ok, err = pcall(sess.archive, sess, ctx, s.session_id or s.path)
		local label = s.title or s.preview or short_id(s.session_id) or "?"
		if ok then
			ui.notify("Archived " .. label)
			return true
		end
		if type(err) == "table" and err.code == "session.partial_failure" then
			ui.notify("Archived " .. label .. " (" .. tostring(err.message or err) .. ")", "warning")
			return true
		end
		ui.notify("Not archived: " .. tostring(type(err) == "table" and err.message or err), "warning")
		return false
	end

	--- archived sessions (`/archive`) of this project, or of all — from the index
	local function archived_list(all)
		refresh.catch_up()
		local rows = mare.store.index_read({ scope = all and "all" or "project", archived = true })
		local out = {}
		for _, e in ipairs(rows) do
			if mare.fs.exists(e.path) then
				out[#out + 1] = e
			end
		end
		return out
	end

	--- unarchive: an archive entry {archived = false} in the session, then its row
	local function unarchive(s)
		local sess = deps.session
		if not sess then
			ui.notify("Not unarchived: session service unavailable", "warning")
			return
		end
		local ok, err = pcall(sess.unarchive, sess, ctx, s.session_id or s.path)
		local label = s.title or s.preview or s.path
		if ok then
			ui.notify("Unarchived " .. label)
			return
		end
		if type(err) == "table" and err.code == "session.partial_failure" then
			ui.notify("Unarchived " .. label .. " (" .. tostring(err.message or err) .. ")", "warning")
			return
		end
		ui.notify("Not unarchived: " .. tostring(type(err) == "table" and err.message or err), "warning")
	end

	local function pick_archived(all)
		local list = archived_list(all)
		if #list == 0 then
			ui.notify("No archived sessions")
			return
		end
		local items = {}
		for i, s in ipairs(list) do
			items[i] = {
				label = s.title or s.preview or (s.path:match("([^/\\]+)%.jsonl$") or s.path),
				detail = relative_time(s.archived_at_ms or s.updated_at_ms or 0)
					.. (all and s.cwd and (" · " .. s.cwd) or ""),
			}
		end
		local i = ui.select({ title = "Archived sessions — Enter unarchives", items = items })
		if i then
			unarchive(list[i])
		end
	end

	--- refuse a command that moves the session position while a turn or session switch runs
	--- the running turn would write its messages under the moved leaf, and a
	--- concurrent switch would fail inside `begin_switch` without surfacing a UI notice
	local function refuse_busy(cmd)
		if ctx.agent:activity().busy then
			ui.notify(("A turn is running — interrupt it first (Esc), then %s"):format(cmd), "warning")
			return true
		end
		if deps.switching and deps.switching() then
			ui.notify(
				("A session switch is already in progress — wait for it to finish, then %s"):format(cmd),
				"warning"
			)
			return true
		end
		return false
	end

	local picker_ui = require("session.picker_ui")

	local function picker_deps()
		return {
			PAGE = PAGE,
			R = R,
			view = view,
			cfg = cfg,
			ctx = ctx,
			S = S,
			load_transcript = deps.load_transcript,
			index_rows = index_rows,
			open_elsewhere = open_elsewhere,
			list_width = list_width,
			project_root = project_root,
			short_map = short_map,
			crashed_marks = crashed_marks,
			short_id = short_id,
			picker_rows = picker_rows,
			picker_items = picker_items,
			session_preview = session_preview,
			scan_crashes = scan_crashes,
			pick_archived = pick_archived,
			archive_session = archive_session,
			delete_session = delete_session,
			tail_cache = tail_cache,
			rename_session = rename_session,
			refuse_busy = refuse_busy,
			after_pick = after_pick,
			options_line = options_line,
		}
	end

	--- /resume picker: reads only the
	--- session index, never session files (the conversation tail of the highlighted row is
	--- read lazily). It opens at once on the index as it is; a background reconcile follows and,
	--- when it changed rows, the open picker's items are replaced in place keeping the highlighted session
	--- . Tabs: this project │ all (←→, h/l); j/k with a count; typing searches
	--- (`re:` regex, `"exact phrase"`; `/` first for a query starting with j/k/h/l/g); Ctrl
	--- keys sort, filter to named sessions, rename, delete (asks first) and list the archive,
	--- so letters always search. `fixed` = rows to show instead (search hits); `search` = the
	--- text the search starts with (`-r <text>`): nothing matching shows
	--- "(no matches)", the first Esc clears it and shows the whole list.
	local function pick_session(all, opts)
		opts = opts or {}
		local P = picker_deps()
		local st = {
			sort = "updated",
			named = false,
			filter = opts.search or "",
			prefill = opts.search ~= nil and opts.search ~= "",
			tab = all and 2 or 1,
			limit = PAGE,
			gen = 0,
		}
		local cur = { maps = {} } -- the maps (and select handle) of the picker on screen
		local function refresh_open()
			picker_ui.refresh_open(P, st, cur, opts)
		end

		local seen = refresh.changes
		local watching = opts.fixed ~= nil
		local synced = watching
		while true do
			st.stale = false
			st.open = nil -- read the peer registry again for each list
			local lists, more, unsupported = picker_ui.read_lists(P, st, opts)
			local spec, maps = picker_ui.build_spec(P, st, cur, opts, lists, more, unsupported)
			if not spec and not synced then
				-- an index that knows no sessions at all may just not have caught up: with
				-- nothing to show yet, wait for the reconcile before saying "No sessions found"
				synced = true
				refresh.reconcile()
				seen = refresh.changes
				lists, more, unsupported = picker_ui.read_lists(P, st, opts)
				spec, maps = picker_ui.build_spec(P, st, cur, opts, lists, more, unsupported)
			end
			if not spec then
				ui.notify(opts.fixed and "No matching sessions" or "No sessions found")
				return
			end
			cur.lists = lists
			if not watching then
				watching = true
				refresh.watch(seen, refresh_open)
			end
			local i, tab, key, filter = picker_ui.show_picker(P, st, cur, spec, maps, refresh_open)
			local done, res = picker_ui.handle_choice(P, st, cur, opts, i, tab, key, filter)
			if done then
				if res then
					return table.unpack(res, 1, res.n)
				end
				return
			end
		end
	end

	--- /resume search <text>: streamed, cancellable (Esc) content search across sessions
	local function search_sessions(query)
		local list = index_list(true)
		local hits, cancelled = {}, false
		local off = ctx:on("key/interrupt", function()
			cancelled = true
		end)
		for i, s in ipairs(list) do
			if cancelled then
				break
			end
			local snip = mare.store.search(s.path, query)
			if snip then
				local h = table.copy(s)
				h.preview = snip -- the match in context, shown in the picker
				hits[#hits + 1] = h
			end
			mare.ui.working(
				("Searching sessions for %q"):format(query),
				("%d/%d · %d found · Esc to stop"):format(i, #list, #hits)
			)
		end
		off()
		mare.ui.working(nil)
		pick_session(true, {
			fixed = hits,
			title = ("Sessions containing %q%s"):format(query, cancelled and " (stopped)" or ""),
		})
	end

	return {
		relative_time = relative_time,
		index_list = index_list,
		resolve_resume = resolve_resume,
		refuse_busy = refuse_busy,
		pick_session = pick_session,
		search_sessions = search_sessions,
		short_id = short_id,
	}
end
