-- session.store: the JSONL writer — entry ids, open / close / reset of the session file,
-- image attachments and `add` (each entry written exactly once);
-- part of the `session` plugin (plugins/session.lua), loaded at the start of its apply.
return function(D)
	local ctx, S, disabled, dir = D.ctx, D.S, D.disabled, D.dir
	local session_guard, poison, poison_reason = mare.task.publish, mare.task.fail_session, mare.task.session_error
	local call_sync = mare.task.call_sync

	local function warn(note)
		pcall(function()
			ctx.logger:warn(note)
		end)
		local connected_ok, connected = pcall(mare.ui.connected)
		if connected_ok and connected then
			local shown = pcall(ui.notify, note, "warning")
			if shown then
				return
			end
		end
		pcall(io.stderr.write, io.stderr, "mare: " .. note .. "\n")
	end

	local function warn_lossy_utf8(lossy)
		if lossy then
			warn("session entry contained non-UTF-8 bytes; replaced lossily with U+FFFD")
		end
	end

	local lib = require("mare_lib")
	local STALE_OWNER_MSG = "the originating session changed before the record was saved"
	local STALE_PERSIST_MSG = "session changed before the agent record could be persisted"

	local function is_tree_entry(e)
		return type(e) == "table" and (e.type == "message" or e.type == "compaction" or e.type == "branch_summary")
	end

	local function known_id(id)
		return (S.known_ids and S.known_ids[id]) or S.entries[id]
	end

	local function check_new_id(ext_id, fail_on_err)
		if ext_id == nil then
			return true
		end
		local err
		if type(ext_id) ~= "string" or ext_id == "" then
			err = "entry id must be a nonempty string"
		elseif known_id(ext_id) then
			err = "duplicate entry id: " .. ext_id
		end
		if err then
			if fail_on_err then
				lib.fail("invalid_arguments", err)
			end
			return nil, err
		end
		return true
	end

	local function current_epoch()
		return (ctx.kernel and ctx.kernel.session_epoch and ctx.kernel:session_epoch()) or S.generation
	end

	local function check_owner(ev, fail_on_stale)
		local epoch = current_epoch()
		if
			(ev.session_id ~= nil or ev.session_epoch ~= nil)
			and (ev.session_id ~= S.id or ev.session_epoch ~= epoch)
		then
			if fail_on_stale then
				lib.fail("stale_session", STALE_PERSIST_MSG)
			end
			return nil, "stale session record owner"
		end
		return { id = S.id, generation = S.generation, session_epoch = epoch }
	end

	local function stale_result()
		return nil, STALE_PERSIST_MSG, nil, nil, "stale_session"
	end

	local function fail_spi(err, qpath, qerr, quarantined_current, quarantined_initial_model, err_code)
		if err_code == "stale_session" or err == STALE_OWNER_MSG or err == STALE_PERSIST_MSG then
			lib.fail("stale_session", tostring(err))
		end
		lib.fail("persist_failed", "session write failed: " .. tostring(err), {
			quarantine_path = qpath,
			quarantine_error = qerr,
			quarantined_current = quarantined_current == true,
			quarantined_initial_model = quarantined_initial_model == true,
		})
	end

	local function new_id()
		-- 16 hex chars (64 bits), retried on a collision with a loaded entry.
		-- A resumed session may have loaded only its tail, so the check can't see older
		-- entries: the id space has to make a collision (and the parent cycle it could
		-- create) negligible on its own
		for _ = 1, 16 do
			local id = mare.crypto.random_hex(8)
			if not known_id(id) then
				return id
			end
		end
		return mare.crypto.random_hex(16)
	end

	--- release the open session file now (lock + in-process registry: a second open of the same
	--- session in this process is refused while a handle is open —)
	local function checked_call(fn, ...)
		local result = table.pack(fn(...))
		if result[1] == false or (result[1] == nil and result.n > 1 and result[2] ~= nil) then
			error(tostring(result[2] or "operation refused"), 0)
		end
		return table.unpack(result, 1, result.n)
	end

	local writer_state, active_writes, drain_waiters, retire_snap = "open", 0, {}, nil
	local commit_mutex = lib.mutex()
	local deleting = false
	local function leave_write()
		active_writes = active_writes - 1
		if active_writes == 0 and next(drain_waiters) then
			local waiters = drain_waiters
			drain_waiters = {}
			for _, ch in ipairs(waiters) do
				ch:send(true)
			end
		end
	end
	local function is_retired()
		return writer_state ~= "open" or deleting or S.close_failed == true
	end

	local function close_file(extra_entries)
		if S.close_failed then
			return nil, "session close previously failed", "failed"
		end
		local f = S.file
		S.file = nil
		if f and f.close then
			if extra_entries then
				for _, entry in ipairs(extra_entries) do
					local ok_extra, extra_ok, extra_err = pcall(f.append, f, entry, { sync = true })
					if not ok_extra or extra_ok == false or (extra_ok == nil and extra_err ~= nil) then
						S.close_failed = true
						pcall(f.close, f)
						return nil,
							"old session close entry failed: " .. tostring(ok_extra and extra_err or extra_ok),
							"failed"
					end
				end
			end
			-- Attempt both steps: neither an append failure nor a close failure proves
			-- the old writer was cleanly retired.
			local ok_append, appended, append_err = pcall(
				f.append,
				f,
				{ type = "state", state = "closed", created_at_ms = lib.to_epoch_ms(mare.task.now()) },
				{ sync = true }
			)
			local ok_close, closed, close_err = pcall(f.close, f)
			if not ok_append or appended == false or (appended == nil and append_err ~= nil) then
				S.close_failed = true
				return nil,
					"old session close marker failed: " .. tostring(ok_append and append_err or appended),
					"failed"
			end
			if not ok_close or closed == false or (closed == nil and close_err ~= nil) then
				S.close_failed = true
				return nil, "old session close failed: " .. tostring(ok_close and close_err or closed), "failed"
			end
			return true, nil, "retired"
		end
		return true, nil, "none"
	end
	local writer_tx, writer_rx = mare.task.channel()
	local pending, writer_closed = {}, false
	local active_worker, draining, retire_waiters = false, false, {}
	local function wake_retire_waiters()
		local rw = retire_waiters
		retire_waiters = {}
		for _, tx in ipairs(rw) do
			tx:send(true)
		end
	end
	local function drain_accepted_writes()
		if not coroutine.isyieldable() or writer_closed then
			return
		end
		if next(pending) ~= nil and not writer_closed then
			local btx, brx = mare.task.channel()
			if writer_tx:send({ barrier = btx }) then
				brx:recv()
			end
		end
		while active_writes > 0 and not writer_closed do
			local tx, rx = mare.task.channel()
			drain_waiters[#drain_waiters + 1] = tx
			rx:recv()
		end
	end
	local function retire_on_shutdown(reason)
		if writer_state == "retired" then
			return
		end
		if draining then
			if coroutine.isyieldable() and not writer_closed then
				local tx, rx = mare.task.channel()
				retire_waiters[#retire_waiters + 1] = tx
				rx:recv()
			end
			if writer_state == "retired" or draining then
				return
			end
		end
		draining = true
		local _drain_guard <close> = setmetatable({}, {
			__close = function()
				draining = false
				wake_retire_waiters()
			end,
		})
		if writer_state == "open" then
			writer_state = "draining"
			local eligible = mare.mode ~= "child"
				and S.id ~= nil
				and S.started == true
				and not S.ended_for_id
				and not S.close_failed
			retire_snap = eligible
					and {
						session_id = S.id,
						cwd = S.cwd or mare.cwd,
						path = S.path or S.planned,
						reason = reason,
					}
				or nil
			drain_accepted_writes()
		elseif reason == "shutdown" and not writer_closed then
			drain_accepted_writes()
		end
		if writer_state == "retired" then
			return
		end
		local snap = retire_snap
		retire_snap = nil
		local closed, close_err, close_outcome = close_file()
		writer_state = "retired"
		if not closed or close_outcome == "failed" then
			S.close_failed = true
			mare.log("warn", "session close failed during " .. tostring(reason) .. ": " .. tostring(close_err))
			return
		end
		if snap and not S.ended_for_id and (close_outcome == "retired" or close_outcome == "none") then
			S.ended_for_id = true
			pcall(ctx.bus.emit, ctx.bus, "session/closed", snap)
		end
	end
	ctx:effect(function()
		return function()
			retire_on_shutdown("reload")
		end
	end, "session file")

	local sw = require("session.store_switch")({
		D = D,
		warn = warn,
		checked_call = checked_call,
		close_file = close_file,
		is_retired = is_retired,
		has_active_writes = function()
			return active_writes > 0
		end,
	})

	local function reset()
		if D.freeze_baseline_cuts then
			D.freeze_baseline_cuts()
		end
		close_file()
		S.unstable = nil
		-- a new or loaded session ends a startup -c / --resume failure (the TUI unblocks)
		S.resume_error = nil
		S.resume_error_code = nil
		S.file, S.path, S.entries, S.known_ids, S.list, S.leaf, S.name = nil, nil, {}, {}, {}, nil, nil
		S.name_revision = 0
		S.step_boundary = nil
		S.labels = {}
		S.tool_states = {}
		S.readonly = nil
		S.lenient = nil
		S.partial = false
		S.pre_tail_custom = nil
		S.pre_tail_branch_custom = nil
		S.msg_count = 0
		S.preview = nil
		S.sandbox_mode = nil
		S.workspace_dirs = nil
		mare.g["session.workspace_dirs"] = nil
		S.model_recs = {}
		S.initial_model_recorded = nil
		S.prunes = {} -- entry id -> replacement content (pruned tool outputs)
		S.queue = nil
		S.operations = {}
		S.pending_request = nil -- the last request marker without a reply
		S.last_state = nil -- the last {type="state"} record read: "running" | "closed"
		S.started = nil
		S.ended_for_id = nil
		S.close_failed = nil
		S.crashed = nil -- the run that wrote this session last ended without a clean close
		S.parent_session_id = nil
		S.cwd = nil
		S.created_at_ms = lib.to_epoch_ms(mare.task.now())
		S.id = sw.new_session_id()
		S.generation = mare.crypto.uuid()
		S.ids = setmetatable({}, { __mode = "k" })
		-- the file is created on the first write, but its path is fixed now so session events
		-- can report it (e.g. for terminal multiplexers that offer `mare -r <id>`)
		S.planned = require("mare_lib").session_file_path(dir, S.id)
		ctx.agent:set_session_id(S.id)
	end

	--- session/changed {session_id, path, reason}: reason = "resume"|"new"|"fork"
	local function changed(reason)
		S.started = true
		sw.commit_identity(S.id, true)
		ctx.bus:emit("session/changed", { session_id = S.id, path = S.path or S.planned, reason = reason })
	end

	local stored_form -- below: the entry as written (images moved to attachment files)
	local function ensure_file(valid)
		if S.readonly then
			error("session is open read-only (another process is writing it)", 0)
		end
		if deleting then
			error("session is being deleted", 0)
		end
		if writer_state == "retired" or (writer_state == "draining" and active_writes == 0) or S.close_failed then
			error("session writer is retired", 0)
		end
		if S.file or disabled then
			return
		end
		local f = mare.store.open(S.planned)
		if valid and not valid() then
			pcall(f.close, f)
			error("session writer request was cancelled or its owner changed", 0)
		end
		-- the header and the entries so far go in as one synced batch; if that fails the
		-- handle (and its lock) is released at once and nothing refers to the file
		local batch = {
			{
				type = "session",
				version = mare.store.format_version,
				session_id = S.id,
				cwd = S.cwd or mare.cwd,
				created_at_ms = lib.to_epoch_ms(S.created_at_ms or mare.task.now()),
				parent_session_id = S.parent_session_id,
			},
		}
		local planned, generation = S.planned, S.generation
		S.path = planned -- attachments of copied (forked) entries go next to it
		for _, e in ipairs(S.list) do
			local stored = stored_form(e, nil, nil, valid)
			-- Attachment writes can yield. A write already issued may still complete,
			-- but cancellation must stop the parent and requested record from committing.
			if valid and not valid() then
				if S.generation == generation and S.path == planned then
					S.path = nil
				end
				pcall(f.close, f)
				error("session writer request was cancelled or its owner changed", 0)
			end
			batch[#batch + 1] = stored
		end
		batch[#batch + 1] = { type = "state", state = "running", created_at_ms = lib.to_epoch_ms(mare.task.now()) }
		local ok, err, lossy = pcall(f.append_batch, f, batch, { sync = true })
		if not ok then
			S.path = nil
			pcall(f.close, f)
			error(err, 0)
		end
		local active, activation_error = pcall(f.activate, f)
		if not active then
			S.path = nil
			pcall(f.close, f)
			error(activation_error, 0)
		end
		S.file = f
		S.started = true
		-- buffered entries reach the file only now: this is where their bytes are replaced
		warn_lossy_utf8(lossy)
	end

	-- pasted images live in `<session>.attachments/` next to the session file, referenced by
	-- path from the JSONL instead of inlined, and are deleted with the session
	local IMG_EXT = { ["image/png"] = "png", ["image/jpeg"] = "jpg", ["image/gif"] = "gif", ["image/webp"] = "webp" }
	local function attachments_dir(path)
		path = path or S.path
		return path and require("mare_lib").attachments_dir(path) or nil
	end
	--- the entry as written: user and tool-result image data moved to attachment files (the
	--- in-memory entry keeps its data); unchanged when there's nothing to move or no session file
	stored_form = function(entry, path, stage, valid)
		local m = entry.type == "message" and entry.message
		if not (m and (m.role == "user" or m.role == "tool_result") and type(m.content) == "table") then
			return entry
		end
		local dir = attachments_dir(path)
		if not dir then
			return entry
		end
		local content, moved = {}, false
		for i, b in ipairs(m.content) do
			content[i] = b
			if b.type == "image" and type(b.data) == "string" and b.data ~= "" then
				if valid and not valid() then
					return nil
				end
				local bytes = mare.crypto.base64_decode(b.data)
				if not bytes then
					error("invalid base64 image data", 0)
				end
				local name = mare.crypto.sha256(bytes) .. "." .. (IMG_EXT[b.mime or ""] or "bin")
				local path = dir .. "/" .. name
				local wrote
				if stage then
					local ok, result = pcall(stage.write, stage, name, bytes)
					wrote = ok and result
				else
					-- A pathname is not proof of a complete object. Always atomically replace
					-- from the verified bytes, syncing the object and its parent directories
					-- before a message may durably refer to it.
					wrote = mare.fs.write(
						path,
						bytes,
						{ mode = 384, atomic = true, sync_boundary = (dir:match("^(.*)[/\\][^/\\]+$") or ".") }
					)
				end
				if valid and not valid() then
					return nil
				end
				if not wrote then
					error("image attachment could not be durably written", 0)
				end
				if wrote then
					local c = {}
					for k, v in pairs(b) do
						c[k] = v
					end
					c.data, c.attachment = nil, name
					content[i], moved = c, true
				end
			end
		end
		if not moved then
			return entry
		end
		local msg = {}
		for k, v in pairs(m) do
			msg[k] = v
		end
		msg.content = content
		local e = {}
		for k, v in pairs(entry) do
			e[k] = v
		end
		e.message = msg
		return e
	end
	--- an entry read back: attachment references get their image data again
	local function hydrate(entry, path)
		local m = entry.type == "message" and entry.message
		if not (m and type(m.content) == "table") then
			return
		end
		local max_attachment_bytes = 32 * 1024 * 1024
		for _, b in ipairs(m.content) do
			if b.type == "image" and b.attachment and not b.data and path then
				local okb, bytes = pcall(mare.store.read_attachment, path, b.attachment, max_attachment_bytes)
				if okb and type(bytes) == "string" then
					b.data = mare.crypto.base64_encode(bytes)
				else
					-- the file is gone or invalid: say so instead of sending an empty image
					b.type, b.text = "text", "[image attachment missing: " .. tostring(b.attachment) .. "]"
					b.attachment = nil
				end
			end
		end
	end

	-- Open and durably stage a new fork without selecting its journal.
	local function prepare_file(snapshot, lease, opts)
		local auxiliary = type(opts) == "table" and opts.auxiliary == true
		if disabled and not (auxiliary and D.recorded_path) then
			return
		end
		snapshot.path = snapshot.planned
		local stage = snapshot.planned:gsub("%.jsonl$", "") .. ".u1-" .. mare.crypto.uuid() .. ".jsonl"
		local has_images = false
		for _, e in ipairs(snapshot.list) do
			local m = e.type == "message" and e.message
			if m and (m.role == "user" or m.role == "tool_result") and type(m.content) == "table" then
				for _, b in ipairs(m.content) do
					if b.type == "image" and type(b.data) == "string" and b.data ~= "" then
						has_images = true
						break
					end
				end
			end
		end
		-- Early checks avoid creating any staging artifacts for known collisions. Exclusive
		-- creation below remains authoritative if either path changes during preparation.
		if mare.fs.exists(snapshot.planned) then
			error("session file already exists: " .. snapshot.planned, 0)
		end
		local att_dir = attachments_dir(snapshot.planned)
		if has_images and att_dir and mare.fs.exists(att_dir) then
			error("session attachment directory already exists: " .. att_dir, 0)
		end
		if has_images then
			lease.stage = mare.store.reserve_staging_attachments(stage)
		end
		local batch = {
			{
				type = "session",
				version = mare.store.format_version,
				session_id = snapshot.id,
				cwd = snapshot.cwd or mare.cwd,
				created_at_ms = lib.to_epoch_ms(snapshot.created_at_ms or mare.task.now()),
				parent_session_id = snapshot.parent_session_id,
			},
		}
		for _, e in ipairs(snapshot.list) do
			batch[#batch + 1] = stored_form(e, stage, lease.stage)
		end
		batch[#batch + 1] = { type = "state", state = "running", created_at_ms = lib.to_epoch_ms(mare.task.now()) }
		-- All yielding attachment writes target the private staging directory. Reserve the final
		-- JSONL and attachment directory only after staging, then install and publish without yield.
		local f = mare.store.open(
			snapshot.planned,
			{ create_new = true, auxiliary = auxiliary, defer_activation = not auxiliary }
		)
		lease.target = f
		if has_images then
			f:install_staged_attachments(lease.stage)
			lease.stage:close()
			lease.stage = nil
		end
		local _, lossy = f:append_batch(batch, { sync = true })
		warn_lossy_utf8(lossy)
		snapshot.file = f
	end

	local function quarantine_failed(entries, reason)
		local state_dir = mare.paths and mare.paths.state
		if not state_dir or state_dir == "" then
			return nil, "missing state directory"
		end
		local lib = require("mare_lib")
		if not S.quarantine or S.quarantine.id ~= S.id or S.quarantine.generation ~= S.generation then
			S.quarantine = {
				id = S.id,
				generation = S.generation,
				path = lib.quarantine_path(state_dir, S.id),
			}
		end
		local qpath, qerr = lib.quarantine_records(S.quarantine.path, entries, reason, state_dir)
		if qpath then
			mare.log(
				"warn",
				("session write failed; quarantined %d record(s) to %s: %s"):format(#entries, qpath, tostring(reason))
			)
		else
			mare.log(
				"error",
				("session write failed and quarantine failed (%s): %s"):format(tostring(qerr), tostring(reason))
			)
		end
		return qpath, qerr
	end

	-- One writer boundary for ordinary entries and atomic input/compaction batches. All
	-- yielding preparation (including attachment staging) stays outside the commit mutex;
	-- tree-parent binding, disk append, and complete publication serialize under `commit_mutex`.
	local function write_batch(entries, options, validate, publication)
		-- Detached, projected copies. Non-UTF-8 values keep their bytes in memory; the file
		-- writer replaces them with U+FFFD and warns when (and only if) they are written.
		local projected = {}
		local ok_proj, proj_err = pcall(function()
			for i, entry in ipairs(entries) do
				local msg_turn_id = type(entry) == "table"
						and entry.type == "message"
						and type(entry.message) == "table"
						and entry.message.turn_id
					or nil
				if msg_turn_id ~= nil then
					local cleaned_entry = table.copy(entry)
					local cleaned_msg = table.copy(entry.message)
					cleaned_msg.turn_id = nil
					cleaned_entry.message = cleaned_msg
					local out = mare.projection.project(cleaned_entry, { preserve_invalid = true })
					if type(out) == "table" and type(out.message) == "table" then
						out.message.turn_id = type(msg_turn_id) == "string"
								and (mare.projection.project(msg_turn_id, { preserve_invalid = true }))
							or msg_turn_id
					end
					projected[i] = out
				else
					projected[i] = mare.projection.project(entry, { preserve_invalid = true })
				end
			end
		end)
		if not ok_proj then
			return nil, proj_err
		end
		entries = projected
		local seen_ids = {}
		for _, entry in ipairs(entries) do
			if entry.entry_id ~= nil then
				if type(entry.entry_id) ~= "string" or entry.entry_id == "" then
					return nil, "entry id must be a nonempty string"
				end
				if seen_ids[entry.entry_id] or known_id(entry.entry_id) then
					return nil, "duplicate entry id: " .. entry.entry_id
				end
				seen_ids[entry.entry_id] = true
			end
		end
		local failure = poison_reason()
		if failure then
			return nil, failure
		end
		if mare.exec.current() then
			return nil, "session publication is unavailable inside a tool call"
		end
		if S.readonly and not options.allow_readonly then
			return nil, "cannot commit to a read-only session"
		end
		if deleting then
			return nil, "session is being deleted", nil, nil, "writer_retired"
		end
		if writer_state == "retired" or (writer_state == "draining" and not active_worker) or S.close_failed then
			return nil, "session writer is retired", nil, nil, "writer_retired"
		end
		active_writes = active_writes + 1
		local _write_guard <close> = setmetatable({}, { __close = leave_write })
		local owner, generation = S.id, S.generation
		local function current()
			return S.id == owner and S.generation == generation and (not validate or validate())
		end
		if not current() then
			return nil, "session transaction became stale", nil, nil, "stale_session"
		end
		local persisting = options.persist ~= false and not S.readonly
		if persisting then
			local ok_ensure, ensure_err = pcall(ensure_file, current)
			if not ok_ensure then
				if options.skip_quarantine then
					return nil, ensure_err
				end
				local qpath, qerr = quarantine_failed(entries, ensure_err)
				return nil, ensure_err, qpath, qerr
			end
		end
		local file = S.file
		local stored = {}
		local prepared, prepare_error = pcall(function()
			for i, entry in ipairs(entries) do
				stored[i] = persisting and stored_form(entry, nil, nil, current) or entry
				if not stored[i] then
					error("session transaction became stale", 0)
				end
			end
		end)
		if not prepared then
			if persisting and not options.skip_quarantine then
				local qpath, qerr = quarantine_failed(entries, prepare_error)
				return nil, prepare_error, qpath, qerr
			end
			return nil, prepare_error
		end
		local function publish_entries()
			for _, entry in ipairs(entries) do
				S.list[#S.list + 1] = entry
				if entry.entry_id then
					S.known_ids = S.known_ids or {}
					S.known_ids[entry.entry_id] = true
					if is_tree_entry(entry) then
						S.entries[entry.entry_id] = entry
					end
				end
				if entry.type == "message" and entry.message and entry.message.role == "user" then
					S.turn_id = entry.entry_id
				end
			end
			if publication then
				publication()
			end
		end
		local ok, err, needs_quarantine, err_code, commit_phase
		do
			-- Serialize yieldable commits through the abort-safe channel mutex while offloading
			-- disk write + fsync to the blocking pool so the UI animation thread never stalls.
			-- `active_writes > 0` fences session switches, deletes, and shutdown.
			local _commit_guard <close> = coroutine.isyieldable() and commit_mutex:acquire() or nil
			local dup_err
			for _, entry in ipairs(entries) do
				if entry.entry_id ~= nil and known_id(entry.entry_id) then
					dup_err = "duplicate entry id: " .. entry.entry_id
					break
				end
			end
			local blocked = poison_reason()
			if dup_err then
				ok, err = nil, dup_err
			elseif blocked then
				ok, err = nil, blocked
			elseif deleting then
				ok, err, needs_quarantine, err_code = nil, "session is being deleted", false, "writer_retired"
			elseif writer_state == "retired" or S.close_failed then
				ok, err, needs_quarantine, err_code = nil, "session writer is retired", false, "writer_retired"
			elseif mare.exec.current() then
				ok, err = nil, "session publication is unavailable inside a tool call"
			else
				local lock_ok, lock_err, lock_code = true, nil, nil
				if type(options.check_in_lock) == "function" then
					lock_ok, lock_err, lock_code = options.check_in_lock()
				end
				if not lock_ok then
					ok, err, needs_quarantine, err_code = nil, lock_err, false, lock_code
				else
					local bind_check = type(options.bind_leaf) == "function" and options.bind_leaf(entries, stored)
						or nil
					local function current_in_lock()
						return current() and (not bind_check or bind_check())
					end
					if persisting and not file and S.file then
						file = S.file
					end
					local async_append = file and file.append_batch_async
					local use_async_io = persisting
						and file
						and type(file) == "userdata"
						and type(async_append) == "function"
						and coroutine.isyieldable()
					if use_async_io then
						if S.file ~= file or not current_in_lock() then
							ok, err = nil, "session transaction became stale"
						else
							if ctx and ctx.kernel and ctx.kernel.mark_call_committed then
								pcall(ctx.kernel.mark_call_committed, ctx.kernel)
							end
							local ok_append, append_err, lossy = pcall(
								checked_call,
								async_append,
								file,
								stored,
								{ sync = options.sync, ack = options.ack }
							)
							if not ok_append then
								ok, err, needs_quarantine = nil, append_err, true
							else
								ok, err, needs_quarantine, err_code, commit_phase = call_sync(function()
									warn_lossy_utf8(lossy)
									local published, publish_error = pcall(session_guard, publish_entries)
									if not published then
										return nil,
											"session committed but publication failed; recovery required: " .. tostring(
												publish_error
											),
											false,
											"post_commit_failed",
											"committed"
									end
									return true
								end)
							end
						end
					else
						ok, err, needs_quarantine, err_code, commit_phase = call_sync(function()
							if S.file ~= file or not current_in_lock() then
								return nil, "session transaction became stale"
							end
							if persisting and file then
								local ok_append, append_err, lossy = pcall(
									checked_call,
									file.append_batch,
									file,
									stored,
									{ sync = options.sync, ack = options.ack }
								)
								if not ok_append then
									return nil, append_err, true
								end
								warn_lossy_utf8(lossy)
							end
							-- A committed batch cannot be rolled back by a failed or yielding replacement
							-- service. The canonical native guard poisons before exposing that failure.
							local published, publish_error = pcall(session_guard, publish_entries)
							if not published then
								return nil,
									"session committed but publication failed; recovery required: " .. tostring(
										publish_error
									),
									false,
									"post_commit_failed",
									"committed"
							end
							return true
						end)
					end
				end
			end
		end
		if not ok and needs_quarantine and persisting and not options.skip_quarantine then
			local qpath, qerr = quarantine_failed(entries, err)
			return nil, err, qpath, qerr, err_code, commit_phase
		end
		if not ok and err == "session transaction became stale" then
			return nil, err, nil, nil, "stale_session", commit_phase
		end
		return ok, err, nil, nil, err_code, commit_phase
	end

	local function add(entry, persist, sync, validate, publication, opts)
		local bind_leaf = opts and opts.bind_leaf or nil
		if bind_leaf == true then
			bind_leaf = function(es, st)
				local leaf = S.leaf
				es[1].parent_entry_id = leaf
				st[1].parent_entry_id = leaf
				return function()
					return es[1].type ~= "message" or S.leaf == leaf
				end
			end
		end
		local parent = entry.parent_entry_id
		local function current()
			if bind_leaf then
				return not validate or validate()
			end
			return (not validate or validate()) and (entry.type ~= "message" or S.leaf == parent)
		end
		return write_batch({ entry }, {
			persist = persist,
			allow_readonly = true,
			skip_quarantine = opts and opts.skip_quarantine or nil,
			bind_leaf = bind_leaf,
			check_in_lock = opts and opts.check_in_lock or nil,
			sync = sync == true
				or entry.type == "message"
				or entry.type == "compaction"
				or (entry.type == "tool_state" and entry.state == "started"),
			ack = entry.type == "tool_state" and entry.state == "started" and entry.tool_call_id or nil,
		}, current, publication)
	end

	-- options (default persist + sync): an ephemeral caller passes { persist = false, sync = false }.
	local function add_batch(entries, validate, publication, options)
		return write_batch(entries, options or { persist = true, sync = true }, validate, publication)
	end

	-- Session-owned file operations sometimes originate in a sandboxed tool (for example a
	-- shell cwd change). This private worker starts with the plugin, outside tool context; it
	-- accepts only closures constructed by session's own modules, never a public escape API.
	if mare.exec.current() then
		error("session writer cannot initialize inside a tool context", 0)
	end
	ctx:effect(function()
		return function()
			writer_closed = true
			for req in pairs(pending) do
				req.reply:send(table.pack(false, "session writer was disposed"))
			end
			local waiters = drain_waiters
			drain_waiters = {}
			for _, ch in ipairs(waiters) do
				ch:send(true)
			end
			wake_retire_waiters()
		end
	end, "session writer queue")
	local function host_valid(owner, completion_token)
		return (writer_state == "open" or writer_state == "draining")
			and not deleting
			and not writer_closed
			and not poison_reason()
			-- During finish_switch (sw.is_transferred()), allow only writes originating from
			-- the active completion coroutine's lease token; all other writes remain blocked.
			and (not sw.is_transferred() or sw.matches_completion(completion_token))
			and (not owner or (owner.id == S.id and owner.generation == S.generation))
	end
	local function valid(req)
		return not req.cancelled and host_valid(req.owner, req.completion_token)
	end
	local function abandon(req)
		if req.abandon and req.result and req.result[1] and req.result[2] then
			pcall(req.abandon, req.result[2])
			req.result = nil
		end
	end
	ctx:spawn(function()
		while true do
			local req = writer_rx:recv()
			if not req then
				return
			end
			if req.barrier then
				req.barrier:send(true)
			elseif req.cleanup then
				if type(req.cleanup) == "function" then
					pcall(req.cleanup)
				else
					abandon(req.cleanup)
				end
			elseif not writer_closed and pending[req] then
				local result
				if not valid(req) then
					result = table.pack(true, nil, STALE_OWNER_MSG, nil, nil, "stale_session")
				else
					active_worker = true
					local _wk_guard <close> = setmetatable({}, {
						__close = function()
							active_worker = false
						end,
					})
					result = table.pack(pcall(req.run, function()
						return valid(req)
					end))
				end
				pending[req] = nil
				req.result = result
				if req.cancelled then
					abandon(req)
				else
					req.reply:send(result)
				end
			end
		end
	end)
	local function run_host(fn, owner, on_abandon)
		local completion_token = sw.completion_token()
		if not mare.exec.current() then
			if writer_state ~= "open" or deleting or S.close_failed then
				return nil, "session writer is retired", nil, nil, "writer_retired"
			end
			if not host_valid(owner, completion_token) then
				return nil, STALE_OWNER_MSG, nil, nil, "stale_session"
			end
			return fn(function()
				return host_valid(owner, completion_token)
			end)
		end
		if writer_closed or writer_state ~= "open" or deleting or not coroutine.isyieldable() then
			return nil, "session writer is unavailable in this tool context"
		end
		local tx, rx = mare.task.channel()
		local req = {
			run = fn,
			owner = owner,
			completion_token = completion_token,
			reply = tx,
			abandon = on_abandon,
		}
		-- Keep active_writes > 0 from acceptance until the caller receives the reply (or cancels)
		-- so drain_accepted_writes() cannot close/emit session/closed while the reply is still
		-- queued for the tool coroutine to resume.
		active_writes = active_writes + 1
		local guard <close> = setmetatable({}, {
			__close = function()
				if not req.delivered then
					req.cancelled = true
					pending[req] = nil
					-- A completed result may contain a resource. Dispose it on the host worker,
					-- never by invoking host operations inside the cancelled tool context.
					if req.result then
						writer_tx:send({ cleanup = req })
					end
				end
				leave_write()
			end,
		})
		pending[req] = true
		if not writer_tx:send(req) then
			pending[req] = nil
			return nil, "session writer is unavailable"
		end
		local result = rx:recv()
		pending[req] = nil
		req.delivered = true
		req.result = nil
		if not result then
			return nil, "session writer stopped before responding"
		end
		if not result[1] then
			error(result[2], 0)
		end
		return table.unpack(result, 2, result.n)
	end

	local function defer_host(fn)
		writer_tx:send({ cleanup = fn })
	end

	-- Do not let a caller-supplied metatable run later in the host worker.
	local function copy_from_tool(value)
		if mare.exec.current() then
			return mare.json.decode(mare.json.encode(value))
		end
		return value
	end

	local RESERVED_RECORD_PREFIXES = {
		"permission/",
		"sandbox/",
		"approval/",
		"agent/",
		"session/",
		"working-directory/",
	}
	local function is_reserved_record_kind(kind)
		if type(kind) ~= "string" then
			return false
		end
		if kind == "job" then
			return true
		end
		for _, prefix in ipairs(RESERVED_RECORD_PREFIXES) do
			if kind:sub(1, #prefix) == prefix then
				return true
			end
		end
		return false
	end

	local function begin_delete(expected_owner)
		sw.require_idle()
		local drained, drain_err = sw.drain_side(nil, expected_owner)
		if not drained then
			return nil, drain_err
		end
		if sw.turn_active() then
			return nil, "Cannot delete session while a turn is running or ending"
		end
		local keep_barrier = false
		deleting = true
		local _barrier_guard <close> = setmetatable({}, {
			__close = function()
				if not keep_barrier then
					deleting = false
				end
			end,
		})
		drain_accepted_writes()
		local closed, close_err = close_file()
		if not closed then
			return nil, close_err
		end
		keep_barrier = true
		return true
	end

	local function cancel_delete()
		deleting = false
	end

	return {
		warn = warn,
		warn_lossy_utf8 = warn_lossy_utf8,
		is_reserved_record_kind = is_reserved_record_kind,
		drain_side = sw.drain_side,
		drain_subagents = sw.drain_subagents,
		require_idle = sw.require_idle,
		is_retired = is_retired,
		turn_active = sw.turn_active,
		switching = sw.switching,
		begin_switch = sw.begin_switch,
		begin_delete = begin_delete,
		cancel_delete = cancel_delete,
		poison_interrupted_completion = sw.poison_interrupted_completion,
		release_switch = sw.release_switch,
		requests_pending = sw.requests_pending,
		new_id = new_id,
		is_tree_entry = is_tree_entry,
		check_new_id = check_new_id,
		current_epoch = current_epoch,
		check_owner = check_owner,
		stale_result = stale_result,
		fail_spi = fail_spi,
		revision = sw.revision,
		verify_revision = sw.verify_revision,
		new_snapshot = sw.new_snapshot,
		prepare_file = prepare_file,
		publish = sw.publish,
		finish_switch = sw.finish_switch,
		close_file = close_file,
		retire_on_shutdown = retire_on_shutdown,
		reset = reset,
		changed = changed,
		ensure_file = ensure_file,
		hydrate = hydrate,
		add = add,
		add_batch = add_batch,
		run_host = run_host,
		defer_host = defer_host,
		copy_from_tool = copy_from_tool,
	}
end
