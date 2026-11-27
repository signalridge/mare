-- changes: per-turn file changes and /rewind. Core keeps a snapshot journal
-- (`mare.snapshot`): before any tool writes, removes or renames a file, the original is saved —
-- whichever edit tool did it. This plugin points the journal at the session, starts a journal
-- turn per agent turn, and offers:
--   /diff files changed in each turn (newest first), with diffs
--   /rewind [turn] put files back to how they were before a turn — optionally the
--                    conversation too (a /tree branch: the old one stays), or fold the
--                    conversation from that turn on into a summary
-- Restores never go through symlinks or hard links; a file changed outside mare since is listed
-- and only restored after confirming; a trimmed backup is reported as missing, never guessed;
-- shell commands' changes can't be snapshotted and the turn says so.
-- Events (emit): changes/rewound {turn, restored, conflicts, missing, refused, failed}

return {
	name = "changes",
	provide = { "changes" },
	inject = { "ui", "bus", "task" },
	config_schema = {
		cap_bytes = {
			type = "integer",
			default = 64 * 1024 * 1024,
			desc = "Cap for stored backups; the oldest turns' backups are trimmed first.",
		},
	},
	apply = function(ctx, cfg)
		-- Subagents share the parent session's SnapshotJournal; never reconfigure, rotate turns,
		-- or clear it from a child VM.
		if mare.mode == "child" then
			return
		end
		-- the optional services, read on each call
		local svc = require("mare_lib").followers(ctx, { "session", "tools", "agent" })
		local turns_meta = {} -- turn id -> {text, shell = bool, n}
		local order = {} -- turn ids, oldest first
		local current
		local configured_dir
		local configured_session_id

		-- no session file (--no-session): a per-run directory under the state dir, removed when
		-- the run ends
		local run_dir = ("%s/snapshots/run-%d-%s"):format(mare.paths.state, mare.pid or 0, mare.crypto.uuid())
		local function journal_dir()
			local info = svc.session and svc.session:info() or {}
			-- a new session has no file until its first message, but its path is already fixed:
			-- the journal goes next to it from the start (deleted and resumed with it)
			local p = info.path or info.planned
			if p then
				return p .. ".snapshots"
			end
			return run_dir
		end
		local function configure()
			local dir = journal_dir()
			local sid = svc.session and svc.session:info().session_id or nil
			if dir ~= configured_dir or sid ~= configured_session_id then
				turns_meta = {}
				order = {}
				configured_dir = dir
				configured_session_id = sid
			end
			mare.snapshot.configure({ dir = dir, cap_bytes = cfg.cap_bytes })
			current = nil
			-- where each turn started in the conversation (for "files and conversation"),
			-- recorded in the session so a resumed session still knows
			if svc.session and svc.session.list_records then
				for _, r in ipairs(require("mare_lib").all_records(svc.session, "changes/turn") or {}) do
					if type(r) == "table" and r.turn then
						local prev = turns_meta[r.turn]
						if not prev then
							turns_meta[r.turn] = { text = r.text, before = r.before, shell = r.shell == true, n = 0 }
						else
							if r.text ~= nil and prev.text == nil then
								prev.text = r.text
							end
							if r.before ~= nil and prev.before == nil then
								prev.before = r.before
							end
							if r.shell == true then
								prev.shell = true
							end
						end
					end
				end
			end
		end
		configure()
		-- the optional session service may come up after this plugin (cordis tree): point the
		-- journal at its file then
		ctx:inject({ "session" }, function()
			configure()
		end)
		ctx:on("session/changed", configure)
		ctx:on("session/loaded", configure)
		ctx:effect(function()
			return function()
				mare.snapshot.configure(nil)
				if mare.fs.exists(run_dir) then
					pcall(mare.fs.remove, run_dir, true)
				end
			end
		end, "cleanup")

		ctx:on("turn/started", function(ev)
			if ev and (ev.trigger == "continue" or ev.trigger == "notification") and current then
				return -- the same turn goes on
			end
			local id = (ev and ev.turn_id) and tostring(ev.turn_id) or tostring(mare.task.now())
			current = id
			order[#order + 1] = id
			-- the conversation point before this turn's prompt (agent/start comes before the
			-- prompt is written): false = the start of the session
			local before = svc.session and svc.session:leaf() or false
			turns_meta[id] = { text = ev and ev.text or nil, shell = false, n = #order, before = before }
			if svc.session and svc.session.add_record then
				local text = ev and ev.text
				pcall(svc.session.add_record, svc.session, "changes/turn", {
					turn = id,
					before = before,
					text = type(text) == "string" and mare.text.cut(text, 2000) or nil,
				})
			end
			mare.snapshot.begin_turn(id)
		end)
		ctx:on("pipeline/tool-dispatching", function(ev)
			local name = ev and ev.call and ev.call.name
			if current and (name == "bash" or name == "powershell" or name == "shell") then
				local meta = turns_meta[current]
				if meta and not meta.shell then
					meta.shell = true
					if svc.session and svc.session.add_record then
						pcall(svc.session.add_record, svc.session, "changes/turn", {
							turn = current,
							shell = true,
						})
					end
				end
			end
		end)

		local rel = require("mare_lib").rel

		local changes = {}
		--- turns() -> journal turns, oldest first: { {turn, label, shell, files = {{path, before, after, missing}}} }
		function changes.turns(_, _)
			local out = {}
			for _, t in ipairs(mare.snapshot.turns() or {}) do
				local m = turns_meta[t.turn] or {}
				if #t.files > 0 then
					out[#out + 1] = {
						turn = t.turn,
						label = m.text and mare.text.trunc_width((m.text:gsub("%s+", " ")), 60) or ("turn " .. t.turn),
						shell = m.shell or t.shell or false,
						files = t.files,
					}
				end
			end
			return out
		end

		--- diffs() -> turns() with each file's own change in that turn (oldest first): `old` = its
		--- backup at the turn's start, `new` = its backup at the next turn that touched it, else the
		--- file now; `adds` / `dels` counted; nothing loaded for `missing` files
		function changes.diffs(_, _)
			local list = changes.turns(changes, ctx)
			for i, t in ipairs(list) do
				for _, f in ipairs(t.files) do
					if not f.missing then
						f.old = f.before and mare.snapshot.content(f.before) or ""
						local nxt
						for j = i + 1, #list do
							for _, g in ipairs(list[j].files) do
								if g.path == f.path then
									nxt = g
									break
								end
							end
							if nxt then
								break
							end
						end
						if nxt then
							f.new = nxt.before and mare.snapshot.content(nxt.before) or ""
						else
							f.new = mare.fs.read(f.path) or ""
						end
						f.adds, f.dels = mare.text.diff_stats(f.old, f.new)
					end
				end
			end
			return list
		end

		--- rewind(turn, {force}) -> result table (restored, conflicts, missing, refused, failed)
		function changes.rewind(_, _, turn, opts)
			opts = opts or {}
			local r = mare.snapshot.restore(turn, opts)
			-- When !force finds conflicts, snapshot.restore writes nothing (preflight only).
			if not opts.force and #(r.conflicts or {}) > 0 then
				return r
			end
			-- restored files changed under the agent: its read records are stale now (otherwise
			-- read-before-edit would judge against content that no longer exists)
			if svc.tools and svc.tools.observe then
				for _, p in ipairs(r.restored or {}) do
					pcall(svc.tools.observe, svc.tools, p)
				end
			end
			ctx.bus:emit("changes/rewound", {
				turn = turn,
				restored = r.restored,
				conflicts = r.conflicts,
				missing = r.missing,
				refused = r.refused,
				failed = r.failed,
			})
			return r
		end
		require("mare_lib").service(ctx, "changes", changes)

		--- one file's change within its turn (from diffs())
		local function file_node(f)
			local head = ui.Line({
				ui.Span(rel(f.path)):hl("MareHeading"),
				ui.Span(("  +%d"):format(f.adds or 0)):hl("MareDiffAddText"),
				ui.Span((" −%d"):format(f.dels or 0)):hl("MareDiffDeleteText"),
			})
			if f.missing then
				return ui.Lines({
					head,
					ui.Line({ ui.Span("  (no backup: too large, a link, or trimmed)"):hl("MareDim") }),
				})
			end
			return ui.Lines({ head, ui.Diff({ old = f.old or "", new = f.new or "", path = rel(f.path) }), ui.Line({}) })
		end

		local function show_diff()
			local list = changes.diffs(changes, ctx)
			if #list == 0 then
				ui.notify("No file changes by tools in this session yet")
				return
			end
			local nodes = {}
			for i = #list, 1, -1 do
				local t = list[i]
				nodes[#nodes + 1] = ui.Line({
					ui.Span(("Turn: %s"):format(t.label)):hl("MareAccent"),
					ui.Span(("  %d file%s"):format(#t.files, #t.files == 1 and "" or "s")):hl("MareDim"),
					t.shell and ui.Span("  · ran shell commands: other changes may not be recorded"):hl("MareWarning")
						or ui.Span(""),
				})
				for _, f in ipairs(t.files) do
					nodes[#nodes + 1] = file_node(f)
				end
			end
			mare.ui.overlay("Changes by turn", ui.Lines(nodes))
		end

		local function report(r, label)
			local parts = { ("Rewound files to before “%s”: %d restored"):format(label, #(r.restored or {})) }
			if #(r.missing or {}) > 0 then
				parts[#parts + 1] = ("%d without a backup (not restored): %s"):format(
					#r.missing,
					table.concat(r.missing, ", ")
				)
			end
			if #(r.refused or {}) > 0 then
				parts[#parts + 1] = ("%d refused (now a link): %s"):format(#r.refused, table.concat(r.refused, ", "))
			end
			if #(r.failed or {}) > 0 then
				local f = {}
				for _, x in ipairs(r.failed) do
					f[#f + 1] = x.path .. " (" .. x.error .. ")"
				end
				parts[#parts + 1] = "failed: " .. table.concat(f, ", ")
			end
			local bad = #(r.missing or {}) + #(r.refused or {}) + #(r.failed or {}) > 0
			ui.notify(table.concat(parts, "; "), bad and "warning" or "info")
		end

		local function current_leaf()
			if svc.session and svc.session.leaf then
				return svc.session:leaf()
			end
			local info = svc.session and svc.session:info() or {}
			return info.leaf_entry_id
		end

		local function journal_sig()
			local turns = mare.snapshot.turns() or {}
			local parts = { journal_dir(), tostring(#turns) }
			for _, t in ipairs(turns) do
				local fp = { tostring(t.turn), tostring(#(t.files or {})) }
				for _, f in ipairs(t.files or {}) do
					fp[#fp + 1] = tostring(f.path)
						.. "="
						.. tostring(f.before)
						.. ":"
						.. tostring(f.after)
						.. ":"
						.. tostring(f.missing)
				end
				parts[#parts + 1] = table.concat(fp, ",")
			end
			return table.concat(parts, "|")
		end

		--- the session position and snapshot state now: /rewind refuses when anything moved while
		--- a picker or confirmation dialog was open
		local function session_stamp()
			local info = svc.session and svc.session:info() or {}
			return table.concat({
				tostring(info.session_id),
				tostring(info.session_epoch),
				tostring(current_leaf()),
				tostring(info.leaf_entry_id),
				tostring(info.count),
				journal_sig(),
			}, "\0")
		end

		local function fresh_turn_meta(turn_id)
			local mem = turns_meta[turn_id]
			if svc.session and svc.session.list_records then
				for _, r in ipairs(require("mare_lib").all_records(svc.session, "changes/turn") or {}) do
					if type(r) == "table" and r.turn == turn_id then
						return {
							text = r.text,
							before = r.before,
							shell = (mem and mem.shell) or r.shell or false,
							n = mem and mem.n or 0,
						}
					end
				end
			end
			return mem or {}
		end

		local function turn_still_present(turn_id)
			for _, t in ipairs(changes.turns(changes, ctx)) do
				if t.turn == turn_id then
					return true
				end
			end
			return false
		end

		local function rewind_ui(arg)
			local stamp = session_stamp()
			local leaf_at_open = current_leaf()
			local list = changes.turns(changes, ctx)
			if #list == 0 then
				ui.notify("Nothing to rewind: no file changes by tools in this session")
				return
			end
			local pick
			if arg and arg ~= "" then
				for _, t in ipairs(list) do
					if t.turn == arg then
						pick = t
					end
				end
				if not pick then
					ui.notify("No such turn: " .. arg, "warning")
					return
				end
			else
				local items = {}
				for i = #list, 1, -1 do
					local t = list[i]
					items[#items + 1] = {
						label = t.label,
						detail = ("%d file%s%s"):format(
							#t.files,
							#t.files == 1 and "" or "s",
							t.shell and " · shell commands ran (may have unrecorded changes)" or ""
						),
						turn = t,
					}
				end
				local i = ui.select({
					title = "Rewind to before which turn? (next: files only, files and conversation, or a summary)",
					items = items,
				})
				if not i then
					return
				end
				pick = items[i].turn
			end
			-- what to rewind: files only, files and the conversation,
			-- or the conversation from here folded into a summary (files stay)
			local meta = fresh_turn_meta(pick.turn)
			local can_talk = svc.session and svc.session.rewind and meta.before ~= nil
			local mode = "files"
			if can_talk then
				local c = ui.select({
					title = ("Rewind to before “%s”"):format(pick.label),
					items = {
						{ label = "Restore files", detail = "the conversation stays" },
						{
							label = "Restore files and the conversation",
							detail = "back to before this turn; its prompt goes to the input",
						},
						{
							label = "Summarize from here",
							detail = "replace this turn and later ones with a summary; files stay",
						},
					},
				})
				if not c then
					return
				end
				mode = ({ "files", "both", "summary" })[c]
			end
			local function ensure_fresh()
				if session_stamp() ~= stamp or current_leaf() ~= leaf_at_open or not turn_still_present(pick.turn) then
					ui.notify("The session changed while /rewind was open — open it again", "warning")
					return false
				end
				if svc.agent and svc.agent:activity().busy then
					ui.notify("A turn is running — interrupt it first (Esc), then /rewind", "warning")
					return false
				end
				return true
			end
			if not ensure_fresh() then
				return
			end
			meta = fresh_turn_meta(pick.turn)
			if mode ~= "files" and meta.before == nil then
				ui.notify("The session changed while /rewind was open — open it again", "warning")
				return
			end
			local function resolve_before_item(target_meta)
				if not (svc.session and svc.session.entries) then
					return nil, "session service unavailable"
				end
				local target_before = target_meta and target_meta.before
				if target_before == nil then
					return nil, "turn metadata no longer in session"
				end
				local entries = svc.session:entries() or {}
				local by_id = {}
				for _, e in ipairs(entries) do
					if type(e) == "table" and type(e.entry_id) == "string" then
						by_id[e.entry_id] = e
					end
				end
				local leaf = svc.session:leaf()
				local chain, cur, seen = {}, leaf, {}
				while type(cur) == "string" and cur ~= "" and not seen[cur] do
					if cur == target_before then
						break
					end
					seen[cur] = true
					local entry = by_id[cur]
					if not entry then
						break
					end
					chain[#chain + 1] = entry
					cur = entry.parent_entry_id
				end
				if target_before ~= false and cur ~= target_before then
					return nil, "turn boundary is no longer on the active branch"
				end
				for i = #chain, 1, -1 do
					local entry = chain[i]
					if
						entry.type == "message"
						and type(entry.message) == "table"
						and entry.message.role ~= "tool_result"
						and type(entry.entry_id) == "string"
					then
						return entry.entry_id, nil
					end
				end
				if target_before == false then
					return nil, nil
				end
				return nil, "no rewindable conversation item found for this turn"
			end
			if mode == "summary" then
				local target_item_id, resolve_err = resolve_before_item(meta)
				if resolve_err then
					ui.notify("Not rewound: " .. tostring(resolve_err), "warning")
					return
				end
				local ok, err = pcall(svc.session.rewind, svc.session, {
					session_id = svc.session:info().session_id,
					before_item_id = target_item_id,
					summarize = true,
				})
				local detail = type(err) == "table" and err.data or nil
				if not ok and not (detail and detail.rewound) then
					ui.notify("Not rewound: " .. tostring(type(err) == "table" and err.message or err), "warning")
				end
				return
			end
			local target_item_id
			if mode == "both" then
				if svc.session:info().readonly then
					ui.notify("Not rewound: session is read-only", "warning")
					return
				end
				local resolve_err
				target_item_id, resolve_err = resolve_before_item(meta)
				if resolve_err then
					ui.notify("Not rewound: " .. tostring(resolve_err), "warning")
					return
				end
			end
			local r = changes.rewind(changes, ctx, pick.turn, {})
			if #(r.conflicts or {}) > 0 then
				local ok = ui.confirm(
					("%d file%s changed outside mare since: %s — restore anyway?"):format(
						#r.conflicts,
						#r.conflicts == 1 and "" or "s",
						table.concat(r.conflicts, ", ")
					)
				)
				if not ok then
					ui.notify("Rewind cancelled: no files were changed", "info")
					return
				end
				if not ensure_fresh() then
					return
				end
				if mode == "both" then
					meta = fresh_turn_meta(pick.turn)
					if meta.before == nil then
						ui.notify("The session changed while /rewind was open — open it again", "warning")
						return
					end
					local resolve_err
					target_item_id, resolve_err = resolve_before_item(meta)
					if resolve_err then
						ui.notify("Not rewound: " .. tostring(resolve_err), "warning")
						return
					end
				end
				local confirmed_conflicts = {}
				for _, path in ipairs(r.conflicts or {}) do
					confirmed_conflicts[path] = true
				end
				local recheck = changes.rewind(changes, ctx, pick.turn, {})
				local extra_conflicts = {}
				for _, path in ipairs(recheck.conflicts or {}) do
					if not confirmed_conflicts[path] then
						extra_conflicts[#extra_conflicts + 1] = path
					end
				end
				if #extra_conflicts > 0 then
					ui.notify(
						("Not rewound: additional files changed outside mare while confirming (%s) — open /rewind again"):format(
							table.concat(extra_conflicts, ", ")
						),
						"warning"
					)
					return
				end
				r = changes.rewind(changes, ctx, pick.turn, { force = true })
			end
			if pick.shell then
				ui.notify("This turn ran shell commands: changes they made may not have been recorded", "warning")
			end
			if mode == "both" then
				meta = fresh_turn_meta(pick.turn)
				if meta.before == nil then
					report(r, pick.label)
					ui.notify("Files restored, conversation not rewound: turn metadata no longer in session", "warning")
					return
				end
				local ok, err = pcall(svc.session.rewind, svc.session, {
					session_id = svc.session:info().session_id,
					before_item_id = target_item_id,
					text = meta.text,
				})
				report(r, pick.label)
				if not ok then
					ui.notify(
						"Files restored, conversation not rewound: "
							.. tostring(type(err) == "table" and err.message or err),
						"warning"
					)
				end
			else
				report(r, pick.label)
			end
		end

		-- registered while the optional `commands` service is up (a child fiber on the cordis tree)
		ctx:inject({ "commands" }, function(ctx)
			ctx.commands:register("diff", {
				description = "File changes by turn",
				auto_execute = true,
				run = function()
					ctx:spawn(show_diff)
				end,
			})
			ctx.commands:register("rewind", {
				description = "Restore files to before a turn (/rewind [turn])",
				args = "[turn]",
				auto_execute = true,
				run = function(args)
					return rewind_ui(args)
				end,
			})
		end)
	end,
}
