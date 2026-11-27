-- session.metadata: public session metadata and lifecycle mutation APIs
-- (update, label_item, archive, unarchive, delete, rewind);
-- part of the `session` plugin (plugins/session.lua), loaded during apply.
return function(D)
	local ctx, S, session = D.ctx, D.S, D.session
	local add = D.store.add
	local index_update = D.history.index_update
	local lib = require("mare_lib")

	local function resolve_any(arg, caller)
		if type(arg) ~= "string" or arg == "" then
			return nil, "session_id must be a non-empty string", "invalid_arguments"
		end
		local cross_project = not mare.contracts.is_peer(caller)
		local path_like = arg:find("[/\\]") or arg:find("%.jsonl$")
		if path_like then
			if not mare.contracts.allows_session_path(caller) then
				return nil, "restricted callers cannot address sessions by file path", "denied"
			end
			if mare.fs.exists(arg) then
				local resolved = mare.fs.resolve(arg)
				local rows = mare.store.index_read({ scope = "all", archived = "any" })
				for _, row in ipairs(rows) do
					if row.path == resolved or row.path == arg then
						return { id = row.session_id, path = row.path }
					end
				end
				local id = (S.path == resolved or S.path == arg) and S.id or nil
				if not id then
					local ok_rc, rc = pcall(mare.store.read_from_compaction, resolved)
					if
						ok_rc
						and type(rc) == "table"
						and type(rc.header) == "table"
						and type(rc.header.session_id) == "string"
					then
						id = rc.header.session_id
					end
				end
				return { id = id, path = resolved }
			end
		end
		local resolved_host_cwd = D.normalize_cwd(mare.cwd)
		local function same_project(cwd)
			return type(cwd) == "string" and cwd ~= "" and D.normalize_cwd(cwd) == resolved_host_cwd
		end
		local rows = mare.store.index_read({ scope = "all", archived = "any" })
		local hits = {}
		for _, row in ipairs(rows) do
			if cross_project or same_project(row.cwd) then
				if row.session_id == arg then
					return { id = row.session_id, path = row.path }
				end
				if type(row.session_id) == "string" and row.session_id:sub(1, #arg) == arg then
					hits[#hits + 1] = row
				end
			end
		end
		if #hits == 1 then
			return { id = hits[1].session_id, path = hits[1].path }
		elseif #hits > 1 then
			return nil, ("session id %q is ambiguous (%d matches)"):format(arg, #hits), "invalid_arguments"
		end
		return nil, ("no session matches %q"):format(arg), "unknown_session"
	end

	local function resolve_target(arg, caller)
		local target, rerr, err_kind = resolve_any(arg, caller)
		if not target then
			lib.fail(err_kind or "unknown_session", rerr or "unknown session")
		end
		return target
	end

	local function is_current(target)
		if target.id and S.id and target.id == S.id then
			return true
		end
		if
			target.path
			and S.path
			and (target.path == S.path or mare.fs.resolve(target.path) == mare.fs.resolve(S.path))
		then
			return true
		end
		return false
	end

	local function annotate(path, entry)
		local okf, f = pcall(mare.store.open, path, { existing = true, auxiliary = true })
		if not okf then
			if tostring(f):find("session locked", 1, true) then
				return nil, "the session is open in another window"
			end
			return nil, tostring(f)
		end
		local ok, err = pcall(f.append, f, entry, { sync = true })
		pcall(f.close, f)
		if not ok then
			return nil, tostring(err)
		end
		local iok, ierr = pcall(mare.store.index_append, { path = path })
		if not iok then
			mare.log("warn", "session index not updated: " .. tostring(ierr))
		end
		return true
	end

	local function archive_children(parent_id, archived, ts, failures, reconciled)
		failures = failures or {}
		if type(parent_id) ~= "string" or parent_id == "" then
			return failures
		end
		if not reconciled then
			local rok, rres = pcall(mare.store.index_reconcile)
			if not rok then
				local msg = "session index reconcile failed: " .. tostring(rres)
				failures[#failures + 1] = { session_id = parent_id, error = msg }
				mare.log("warn", msg)
			elseif type(rres) == "table" and ((rres.failed or 0) > 0 or (rres.pending or 0) > 0) then
				local msg = ("session index reconcile incomplete (failed=%d, pending=%d)"):format(
					rres.failed or 0,
					rres.pending or 0
				)
				failures[#failures + 1] = { session_id = parent_id, error = msg }
				mare.log("warn", msg)
			end
		end
		local ok, rows = pcall(mare.store.index_read, {
			scope = "all",
			archived = "any",
			kind = "agent",
			parent_session_id = parent_id,
		})
		if not ok or type(rows) ~= "table" then
			failures[#failures + 1] = {
				session_id = parent_id,
				error = type(rows) == "string" and rows or "failed to list child sessions",
			}
			mare.log("warn", ("child session archive list (%s): %s"):format(tostring(parent_id), tostring(rows)))
			return failures
		end
		for _, row in ipairs(rows) do
			if type(row.path) == "string" and row.path ~= "" then
				local aok, aerr = annotate(row.path, { type = "archive", archived = archived, created_at_ms = ts })
				if not aok then
					failures[#failures + 1] = {
						session_id = row.session_id,
						path = row.path,
						error = tostring(aerr),
					}
					mare.log(
						"warn",
						("child session archive (%s): %s"):format(tostring(row.session_id), tostring(aerr))
					)
				end
				archive_children(row.session_id, archived, ts, failures, true)
			end
		end
		return failures
	end

	local function format_child_failures(failures, verb)
		local parts = {}
		for _, f in ipairs(failures) do
			parts[#parts + 1] = ("%s (%s)"):format(tostring(f.session_id or f.path or "?"), tostring(f.error))
		end
		return ("%s parent, but %d child session(s) failed: %s"):format(verb, #failures, table.concat(parts, "; "))
	end

	local function lock_or_persist_code(err)
		local msg = tostring(err)
		if
			msg:find("open in another window", 1, true)
			or msg:find("session locked", 1, true)
			or msg:find("already open in this process", 1, true)
		then
			return "busy"
		end
		return "persist_failed"
	end

	local function delete_files(path, id)
		local ok, err = mare.store.delete(path)
		if not ok and not tostring(err):find("session file not found", 1, true) then
			return nil, err
		end
		local tok, terr = pcall(mare.store.index_append, { session_id = id, path = path, deleted = true })
		if not tok then
			mare.log("warn", "session index row not removed: " .. tostring(terr))
		end
		return true
	end

	function session.update(_, _, name, opts)
		if type(name) ~= "string" then
			lib.fail("invalid_arguments", "session name must be a string")
		end
		if opts ~= nil and type(opts) ~= "table" then
			lib.fail("invalid_arguments", "session.update options must be a table")
		end
		local expected = opts and opts.expected_revision
		if expected ~= nil then
			if type(expected) ~= "number" or expected < 0 or expected % 1 ~= 0 then
				lib.fail("invalid_arguments", "expected_revision must be a non-negative integer")
			end
			local current_rev = S.name_revision or 0
			if current_rev ~= expected then
				lib.fail(
					"session.revision_conflict",
					("session revision %d does not match expected_revision %d"):format(current_rev, expected)
				)
			end
		end
		local ok, err, err_code = D.set_name(name, expected)
		if not ok then
			lib.fail(err_code == "session.revision_conflict" and err_code or "persist_failed", tostring(err))
		end
		return D.current_session_snapshot()
	end

	function session.label_item(_, _, label, opts)
		if type(label) ~= "string" then
			lib.fail("invalid_arguments", "label must be a string")
		end
		if opts ~= nil and type(opts) ~= "table" then
			lib.fail("invalid_arguments", "session.label_item options must be a table")
		end
		if opts and opts.item_id ~= nil and (type(opts.item_id) ~= "string" or opts.item_id == "") then
			lib.fail("invalid_arguments", "item_id must be a non-empty string")
		end
		if ctx.agent and ctx.agent:activity().busy then
			lib.fail("busy", "a turn is running — interrupt it first")
		end
		D.history.ensure_full()
		local target = (opts and opts.item_id) or S.leaf
		if not target then
			lib.fail("session.unknown_item", "nothing to label yet")
		end
		local target_entry, entry_id = D.resolve_item_entry(S, target)
		if not target_entry then
			lib.fail("session.unknown_item", "no such session item: " .. tostring(target))
		end
		local ok, err = add({
			type = "label",
			target_entry_id = entry_id,
			label = label,
			created_at_ms = lib.to_epoch_ms(mare.task.now()),
		})
		if not ok then
			lib.fail("persist_failed", "session write failed: " .. tostring(err))
		end
		S.labels[entry_id] = label ~= "" and label or nil
		index_update()
		return {
			item_id = target,
			label = S.labels[entry_id],
		}
	end

	function session.archive(_, caller, session_id)
		local target = resolve_target(session_id, caller)
		if is_current(target) then
			lib.fail("session.active_session", "cannot archive the active session; switch away or close it first")
		end
		local ts = lib.to_epoch_ms(mare.task.now())
		local ok, err = annotate(target.path, { type = "archive", archived = true, created_at_ms = ts })
		if not ok then
			lib.fail(lock_or_persist_code(err), tostring(err))
		end
		local failures = archive_children(target.id, true, ts)
		if #failures > 0 then
			lib.fail("session.partial_failure", format_child_failures(failures, "archived"), {
				failed_children = failures,
			})
		end
		return {}
	end

	function session.unarchive(_, caller, session_id)
		local target = resolve_target(session_id, caller)
		local ts = lib.to_epoch_ms(mare.task.now())
		local ok, err = annotate(target.path, { type = "archive", archived = false, created_at_ms = ts })
		if not ok then
			lib.fail(lock_or_persist_code(err), tostring(err))
		end
		local failures = archive_children(target.id, false, ts)
		if #failures > 0 then
			lib.fail("session.partial_failure", format_child_failures(failures, "unarchived"), {
				failed_children = failures,
			})
		end
		return {}
	end

	local function delete_and_emit(path, id)
		local ok, err = delete_files(path, id)
		if not ok then
			return nil, err
		end
		if type(id) == "string" and id ~= "" then
			ctx.bus:emit("session/deleted", { session_id = id, path = path })
		end
		return true
	end

	function session.delete(_, caller, session_id)
		local target = resolve_target(session_id, caller)
		if is_current(target) then
			lib.fail("session.active_session", "cannot delete the active session; switch away or close it first")
		end
		local ok, err = delete_and_emit(target.path, target.id)
		if not ok then
			lib.fail(lock_or_persist_code(err), tostring(err))
		end
		return {}
	end

	function session.rewind(_, caller, opts)
		if opts ~= nil and type(opts) ~= "table" then
			lib.fail("invalid_arguments", "session.rewind options must be a table")
		end
		opts = opts or {}
		if opts.session_id ~= nil then
			if type(opts.session_id) ~= "string" or opts.session_id == "" then
				lib.fail("invalid_arguments", "session_id must be a non-empty string")
			end
			if opts.session_id ~= S.id then
				lib.fail("unknown_session", ("session `%s` is not open in this process"):format(opts.session_id))
			end
		end
		D.check_expected_epoch(opts.expected_epoch)
		local before_item_id = opts.before_item_id
		local before_entry_id = false
		if before_item_id ~= nil then
			if type(before_item_id) ~= "string" or before_item_id == "" then
				lib.fail("invalid_arguments", "before_item_id must be a non-empty string")
			end
			D.history.ensure_full()
			local target_entry = D.resolve_item_entry(S, before_item_id)
			if not target_entry then
				lib.fail("session.unknown_item", "no such session item: " .. tostring(before_item_id))
			end
			before_entry_id = target_entry.parent_entry_id or false
		end
		local restore_files = opts.restore_files == true
		local changes_turn
		if restore_files then
			local changes = D.follow.changes and D.follow.changes()
			if not changes or type(changes.rewind) ~= "function" then
				lib.fail("unsupported", "file restoration requires the changes service")
			end
			if ctx.agent:activity().busy then
				lib.fail("busy", "a turn is running — interrupt it first")
			end
			if S.readonly then
				lib.fail("persist_failed", "session is read-only")
			end
			local target_before = before_entry_id
			local changes_records = lib.all_records({
				list_records = function(_, k, o)
					return session.list_records(session, ctx, k, o)
				end,
			}, "changes/turn", { branch = "current" }) or {}
			for _, rec in ipairs(changes_records) do
				if type(rec) == "table" and rec.before == target_before and type(rec.turn) == "string" then
					changes_turn = rec.turn
				end
			end
			if not changes_turn then
				lib.fail("unsupported", "restore_files needs a turn boundary; no file snapshot starts at this item")
			end
			if changes_turn then
				local r = changes:rewind(changes_turn, {})
				if type(r) == "table" and #(r.conflicts or {}) > 0 then
					lib.fail(
						"session.file_conflict",
						"files changed outside mare since the target turn; rewind aborted",
						{
							conflicts = r.conflicts,
							turn = changes_turn,
						}
					)
				end
				if
					type(r) == "table" and (#(r.failed or {}) > 0 or #(r.refused or {}) > 0 or #(r.missing or {}) > 0)
				then
					lib.fail("persist_failed", "failed to restore files before rewind", {
						failed = r.failed,
						refused = r.refused,
						missing = r.missing,
						restored = r.restored,
						turn = changes_turn,
					})
				end
			end
		end
		if ctx.kernel and ctx.kernel.mark_call_committed then
			ctx.kernel:mark_call_committed()
		end
		local ok, err, detail = D.rewind.rewind_conversation(caller, before_entry_id, {
			summarize = opts.summarize == true,
			text = opts.text,
		})
		if not ok then
			if type(err) == "table" and type(err.code) == "string" then
				error(err, 0)
			end
			local msg = tostring(err or "session.rewind failed")
			if type(detail) == "table" and detail.rewound then
				lib.fail("unavailable", msg, {
					rewound = true,
					session = D.current_session_snapshot(),
					leaf_entry_id = S.leaf,
					summarize_error = detail.summarize_error,
				})
			elseif msg:find("turn is running", 1, true) then
				lib.fail("busy", msg)
			elseif msg:find("cancelled", 1, true) or msg:find("aborted", 1, true) then
				lib.fail("cancelled", msg)
			else
				lib.fail("persist_failed", msg)
			end
		end
		return D.current_session_snapshot()
	end

	return {
		annotate = annotate,
		archive_children = archive_children,
		delete_and_emit = delete_and_emit,
		delete_files = delete_files,
		format_child_failures = format_child_failures,
		resolve_any = resolve_any,
	}
end
