-- Helper functions for session/lifecycle.lua's load_opened flow.
local M = {}

local function one_line_err(err)
	local s = tostring(err)
	s = s:match("^(.-)\nstack traceback:") or s
	return (s:gsub("[\r\n]+", " "))
end
M.one_line_err = one_line_err

--- Prompt or fail when opening a locked or corrupt session file that could not be opened directly.
function M.resolve_unopened_mode(D, path, known, f)
	local msg = one_line_err(f)
	local here = msg:find("session is already open in this process", 1, true) ~= nil
	local locked = here or msg:find("session locked", 1, true) ~= nil
	local corrupt = msg:find("session file is corrupt", 1, true) ~= nil
	if not (locked or corrupt) then
		error(f, 0)
	end
	local hint = here and "this session is already open in this mare process (e.g. by a sub-agent)"
		or corrupt and ("this session file is damaged and was left untouched (" .. msg .. ")")
		or "this session is open in another mare process"
	if not mare.ui.connected() then
		if locked and not corrupt and mare.args and mare.args.fork_session then
			local read_ok, err = pcall(D.read_validated_fork_snapshot, path, known and known.session_id or nil)
			if not read_ok then
				error(tostring(err), 0)
			end
			return true, false, false
		end
		require("mare_lib").fail(
			locked and "busy" or "corrupt",
			hint
				.. (
					locked and ": close the other writer and reopen, or view read-only in the TUI"
					or ": back up or repair the source, or view read-only in the TUI"
				)
		)
	end
	local items = {}
	local fork_available = locked and not corrupt
	local snapshot_error
	if fork_available then
		-- A lock error may hide a damaged source. Offer Fork only after a strict stable
		-- snapshot and completed-turn validation; the chosen action reads its own snapshot.
		local read_ok, err = pcall(D.read_validated_fork_snapshot, path, known and known.session_id or nil)
		if not read_ok then
			snapshot_error = tostring(err)
		end
		if snapshot_error then
			fork_available = false
			if snapshot_error:find("session file is corrupt", 1, true) then
				corrupt = true
			elseif snapshot_error:find("id mismatch", 1, true) then
				error("Cannot fork: " .. snapshot_error, 0)
			end
		end
	end
	if corrupt and snapshot_error then
		hint = "this session file is damaged and was left untouched (" .. snapshot_error .. ")"
	elseif snapshot_error then
		hint = hint
			.. ": a stable completed snapshot could not be read; view read-only or retry after the writer finishes"
	end
	if fork_available then
		items[#items + 1] = { label = "Fork into a new session", detail = "use the last completed snapshot" }
	end
	items[#items + 1] = {
		label = "View read-only",
		detail = corrupt and "unreadable lines are skipped; nothing is written"
			or "nothing is written; tools with side effects won't run",
	}
	local i = ui.select({ title = hint, items = items })
	if i == nil then
		require("mare_lib").fail("cancelled", "session switch cancelled")
	end
	local fork_it = fork_available and i == 1
	local readonly = not fork_it
	return fork_it, readonly, corrupt
end

--- Read backwards to the latest compaction only, or read whole damaged/forked files.
function M.read_session_tail(D, path, known, opened, f, lease, fork_it, lenient)
	local tail
	local skipped = 0
	-- Lock-free reads (another writer, or a damaged file) are stat-checked: a file that keeps
	-- changing may still roll back its last batch.
	local stable = true
	if not opened then
		local all, nskipped
		if fork_it then
			all = mare.store.read_fork_snapshot(path, known and known.session_id or nil)
			D.validate_fork_source(all, path)
		else
			all, nskipped = mare.store.read_bound(path, lenient)
		end
		skipped = nskipped or 0
		local h = all[1]
		tail = { entries = all, partial = false, header = (h and h.type == "session") and h or nil }
	else
		tail = mare.store.read_tail_snapshot(f)
		lease.target_revision = tail.revision
		stable = tail.stable ~= false
	end
	if not opened and not stable and fork_it then
		error("this session is being written right now and its end may still change — fork it again in a moment", 0)
	end
	return tail, skipped, stable
end

--- Resolve resume_cwd and shell_cwd when the session was recorded in another directory.
function M.resolve_resume_cwd(cfg, tail, path, opened, f, reuse, lease, snap, readonly)
	local rec = tail.header and tail.header.cwd
	local preparation_warnings = {}
	local last_to
	for _, e in ipairs(tail.entries or {}) do
		if e.type == "custom" and e.kind == "working-directory/changed" and e.data then
			last_to = e.data.to
		end
	end
	local readonly_opened = false
	snap.resume_cwd = nil
	if type(rec) == "string" and rec ~= mare.cwd and not readonly then
		local st = mare.fs.stat(rec)
		local exists = st and st.kind == "dir"
		local mode = cfg.resume_cwd
		if mode == "ask" and mare.ui.connected() then
			local items = {}
			if exists then
				items[#items + 1] = { label = "Use the session's directory", detail = rec }
			end
			items[#items + 1] = { label = "Use the current directory", detail = mare.cwd }
			if not exists then
				items[#items + 1] = { label = "Choose another directory", detail = "enter a directory path" }
			end
			items[#items + 1] = { label = "View read-only", detail = "nothing is written" }
			while true do
				local i = ui.select({
					title = exists and "This session was recorded in another directory"
						or ("This session's directory no longer exists: " .. rec),
					items = items,
				})
				if i == nil then
					require("mare_lib").fail("cancelled", "session switch cancelled")
				end
				local pick = items[i].label
				if pick == "View read-only" then
					readonly = true
					if opened then
						readonly_opened = true
						lease.source_file, lease.source_path, lease.source_revision = f, path, tail.revision
						lease.discard_target = not reuse
					end
					break
				elseif pick == "Use the session's directory" then
					snap.resume_cwd = rec
					break
				elseif pick == "Choose another directory" then
					local entered = ui.input({
						title = "Directory to resume in",
						placeholder = mare.cwd,
					})
					if entered ~= nil then
						local trimmed = entered:match("^%s*(.-)%s*$")
						if trimmed ~= "" then
							local chosen = mare.fs.resolve(trimmed)
							local cst = mare.fs.stat(chosen)
							if cst and cst.kind == "dir" then
								snap.resume_cwd = chosen
								break
							end
							ui.notify("Not a directory: " .. chosen, "warning")
						end
					end
				else
					break
				end
			end
		elseif mode == "session" and exists then
			snap.resume_cwd = rec
		end
		if not exists and not snap.resume_cwd and not readonly then
			-- Never fall back silently: say where it runs instead.
			local note = ("the session's directory %s no longer exists; continuing in %s"):format(rec, mare.cwd)
			preparation_warnings[#preparation_warnings + 1] = note
		end
	end
	-- Where the shell starts: the last recorded directory when resuming in the session's
	-- own directory (or when it was chosen), otherwise the launch directory — and only
	-- while that directory still exists.
	snap.restore_cwd = rec == mare.cwd or snap.resume_cwd ~= nil
	snap.shell_cwd = nil
	if rec == mare.cwd or snap.resume_cwd then
		snap.shell_cwd = last_to or snap.resume_cwd
		local st = snap.shell_cwd and mare.fs.stat(snap.shell_cwd)
		if snap.shell_cwd and not (st and st.kind == "dir") then
			local note = ("the shell's last directory %s no longer exists; it starts in %s"):format(
				snap.shell_cwd,
				snap.resume_cwd or mare.cwd
			)
			preparation_warnings[#preparation_warnings + 1] = note
			snap.shell_cwd = snap.resume_cwd
		end
	end
	return readonly, readonly_opened, preparation_warnings
end

--- Apply tail entries to the snapshot, handle partial compaction fallback, and prepare fork if needed.
function M.hydrate_snapshot(
	D,
	ctx,
	disabled,
	snap,
	history,
	tail,
	path,
	known,
	opened,
	f,
	lease,
	fork_it,
	readonly,
	readonly_opened,
	stable
)
	local entries = tail.entries
	local context_for, apply_entries, tail_covers_leaf =
		history.context_for, history.apply_entries, history.tail_covers_leaf
	snap.msg_count = known and known.messages or 0
	snap.preview = known and known.preview or nil
	if not known then
		for _, e in ipairs(entries) do
			if e.type == "message" then
				snap.msg_count = snap.msg_count + 1
			end
		end
	end
	apply_entries(entries, fork_it)
	-- The tail's current branch must include its compaction ancestry.
	if snap.partial and (readonly_opened or fork_it or not tail_covers_leaf()) then
		snap.entries, snap.list = {}, {}
		local all, _, _, st
		if opened then
			all, st = mare.store.read_snapshot(f, tail.revision), true
		else
			all, _, _, st = mare.store.read(path)
		end
		apply_entries(all)
		snap.partial = false
		if st == false then
			stable = false
			if not opened and fork_it then
				error(
					"this session is being written right now and its end may still change — fork it again in a moment",
					0
				)
			end
		end
	end
	local precomputed_settings, usage_stale
	if snap.partial then
		-- Inbox snapshots, message commit identities, and branch settings can precede
		-- the compaction tail. Replay a detached projection without expanding live history.
		local inbox = D.store.new_snapshot()
		inbox.restore_cwd, inbox.resume_cwd = snap.restore_cwd, snap.resume_cwd
		local all = opened and mare.store.read_snapshot(f, tail.revision) or mare.store.read(path)
		local inbox_history = require("session.history")({ ctx = ctx, S = inbox, disabled = disabled, store = D.store })
		inbox_history.apply_entries(all, true)
		snap.queue = inbox.queue
		snap.operations = inbox.operations or {}
		snap.name, snap.name_revision = inbox.name, inbox.name_revision
		snap.known_ids = inbox.known_ids or {}
		local tail_custom_ids = {}
		for _, e in ipairs(snap.list) do
			if e.type == "custom" and e.entry_id then
				tail_custom_ids[e.entry_id] = true
			end
		end
		local pre_tail_custom = {}
		local tail_start = math.max(1, #inbox.list - #snap.list + 1)
		for i = 1, tail_start - 1 do
			local e = inbox.list[i]
			if e.type == "custom" and not (e.entry_id and tail_custom_ids[e.entry_id]) then
				pre_tail_custom[#pre_tail_custom + 1] = e
			end
		end
		local tail_branch_ids, tail_branch_n = {}, 0
		for _, e in ipairs(history.events_for(snap.leaf)) do
			if e.type == "custom" then
				tail_branch_n = tail_branch_n + 1
				if e.entry_id then
					tail_branch_ids[e.entry_id] = true
				end
			end
		end
		local branch_custom = {}
		for _, e in ipairs(inbox_history.events_for(snap.leaf)) do
			if e.type == "custom" then
				branch_custom[#branch_custom + 1] = e
			end
		end
		local pre_tail_branch_custom = {}
		local pre_branch_limit = math.max(0, #branch_custom - tail_branch_n)
		local tail_branch_misaligned = false
		for i, e in ipairs(branch_custom) do
			if (e.entry_id and not tail_branch_ids[e.entry_id]) or (not e.entry_id and i <= pre_branch_limit) then
				if e.entry_id and tail_custom_ids[e.entry_id] then
					tail_branch_misaligned = true
					break
				end
				pre_tail_branch_custom[#pre_tail_branch_custom + 1] = e
			end
		end
		if not tail_branch_misaligned then
			-- Also catch un-ID'd tail custom entries anchored on a pre-compaction ancestor.
			local inbox_on_path, tail_on_path = {}, {}
			for _, e in ipairs(inbox_history.path_to(snap.leaf)) do
				inbox_on_path[e.entry_id] = true
			end
			for _, e in ipairs(history.path_to(snap.leaf)) do
				tail_on_path[e.entry_id] = true
			end
			for _, e in ipairs(snap.list) do
				if
					e.type == "custom"
					and e.at_entry_id
					and inbox_on_path[e.at_entry_id]
					and not tail_on_path[e.at_entry_id]
				then
					tail_branch_misaligned = true
					break
				end
			end
		end
		if tail_branch_misaligned then
			snap.pre_tail_custom = nil
			snap.pre_tail_branch_custom = nil
		else
			snap.pre_tail_custom = pre_tail_custom
			snap.pre_tail_branch_custom = pre_tail_branch_custom
		end
		precomputed_settings = D.prepare_settings(snap.leaf, inbox_history, inbox)
		usage_stale = require("mare_lib").path_usage_stale(inbox_history.path_to(snap.leaf), inbox.list, false)
	end
	if not opened and not stable then
		snap.unstable = true
	end
	local made_fork = fork_it
	local msgs
	if made_fork then
		local before_id = mare.args and mare.args.fork_before_entry or nil
		local keep_all = before_id == nil
		local fork_opts = mare.args and { id = mare.args.session_id, name = mare.args.name } or nil
		snap = D.completed_fork_source(snap, before_id, keep_all)
		local verified
		if type(mare.args) == "table" and type(mare.args.fork_images) == "table" then
			verified = mare.args.fork_images
			mare.args.fork_images = nil
		end
		if verified then
			D.hydrate_verified_fork_images(snap, verified)
		end
		snap, msgs = D.prepare_fork(snap, nil, true, fork_opts)
		D.store.prepare_file(snap, lease)
		f, path, opened, readonly = snap.file, snap.path, snap.file ~= nil, false
	else
		msgs = context_for(snap.leaf)
	end
	if usage_stale == nil then
		local path_fn = made_fork
				and require("session.history")({ ctx = ctx, S = snap, disabled = disabled, store = D.store }).path_to
			or history.path_to
		usage_stale = require("mare_lib").path_usage_stale(path_fn(snap.leaf), snap.list, false)
	end
	if readonly_opened then
		opened = false
	end
	snap.file = opened and f or nil
	snap.readonly = readonly or nil
	snap.crashed = opened and not made_fork and snap.last_state ~= "closed" or nil
	return snap, msgs, f, path, opened, readonly, made_fork, precomputed_settings, usage_stale
end

--- Run post-publish finish_switch notifications, state appends, and lifecycle events.
function M.run_finish_switch(D, ctx, S, critical_call, changed, lease, opts)
	local preparation_warnings = opts.preparation_warnings
	local opened, made_fork, f, readonly = opts.opened, opts.made_fork, opts.f, opts.readonly
	local msgs, path, skipped = opts.msgs, opts.path, opts.skipped
	local fork_it, source_path = opts.fork_it, opts.source_path
	return D.store.finish_switch(lease, function(attempt)
		attempt(function()
			for _, note in ipairs(preparation_warnings) do
				mare.log("warn", note)
				if not mare.ui.connected() then
					io.stderr:write("mare: " .. note .. "\n")
				end
			end
			if S.unstable and mare.ui.connected() then
				ui.notify(
					"Unstable: this session is being written right now — the read-only view may be incomplete",
					"warning"
				)
			end
		end)
		attempt(function()
			if opened and not made_fork then
				-- No clean close after the last open: that run crashed or was killed (an older
				-- file without markers counts as one too — it can't prove otherwise).
				critical_call(f.append, f, {
					type = "state",
					state = "running",
					created_at_ms = require("mare_lib").to_epoch_ms(mare.task.now()),
				})
			end
		end, true)
		attempt(function()
			if readonly and mare.ui.connected() then
				ui.notify(require("session.readonly")().notice, "info")
			end
		end)
		attempt(function()
			D.restore_settings(
				S.leaf,
				opts.precomputed_settings,
				opts.restore_ceiling and opts.restore_ceiling.sandbox or nil
			)
		end, true)
		attempt(function()
			-- Inputs queued when the process stopped run as follow-ups.
			if S.queue and ((#(S.queue.steer or {}) + #(S.queue.follow_up or {})) > 0) and mare.ui.connected() then
				ui.notify("Restored queued messages from the last run — they run after your next prompt", "info")
			end
		end)
		attempt(function()
			local ag = ctx.agent
			ctx.bus:emit("agent/messages-replaced", { messages = ag:messages() })
		end)
		-- A request that never got a reply (crash mid-request): the agent records `aborted`
		-- and retries it with the same request id.
		attempt(function()
			if S.pending_request and S.file and ctx.agent.set_pending_request then
				critical_call(ctx.agent.set_pending_request, ctx.agent, S.pending_request, { clean = not S.crashed })
				S.pending_request = nil
			else
				S.pending_request = nil
			end
		end, true)
		attempt(function()
			changed(made_fork and "fork" or "resume")
		end)
		attempt(function()
			local ag = ctx.agent
			local current_msgs = ag:messages()
			local compacted = current_msgs ~= msgs or nil
			D.emit_loaded({
				messages = current_msgs,
				path = path,
				name = S.name,
				shell_cwd = S.shell_cwd,
				compacted = compacted,
				usage_stale = compacted == true or opts.usage_stale == true,
				restore_permission_ceiling = opts.restore_ceiling and opts.restore_ceiling.permission or nil,
				restore_sandbox_ceiling = opts.restore_ceiling and opts.restore_ceiling.sandbox or nil,
			})
		end)
		attempt(function()
			if ctx.agent:interrupted() and mare.ui.connected() then
				ui.notify("The last turn was interrupted — /continue resumes it from here", "warning")
			end
		end)
		attempt(function()
			-- Never skip damaged records silently: say how many were left out.
			if skipped > 0 then
				local what = fork_it and "the fork keeps only the history before the first damaged record"
					or "read-only view"
				if mare.ui.connected() then
					ui.notify(
						("Skipped %d damaged record%s — %s"):format(skipped, skipped == 1 and "" or "s", what),
						"warning"
					)
				end
			end
		end)
		if skipped > 0 then
			attempt(function()
				ctx.bus:emit("session/damaged", { path = source_path, skipped = skipped, fork = fork_it })
			end)
		end
		return msgs
	end)
end

return M
