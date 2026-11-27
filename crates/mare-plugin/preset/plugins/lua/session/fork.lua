-- session.fork: moving the leaf (set_leaf) and forking into a new session (session:fork);
-- part of the `session` plugin (plugins/session.lua), loaded at the start of its apply.
return function(D)
	local ctx, S, dir, session = D.ctx, D.S, D.dir, D.session
	local changed, add = D.store.changed, D.store.add
	local context_for, index_update, ensure_full = D.history.context_for, D.history.index_update, D.history.ensure_full
	local maybe_yield = require("mare_lib").maybe_yield
	local DENIED = {
		__name = "mare.Error",
		__tostring = function(e)
			return e.code .. ": " .. e.message
		end,
	}
	local function authorize_tree_settings(caller)
		if not D.follow.llm() then
			return
		end
		for _, operation in ipairs({ "set_model", "set_effort" }) do
			if not mare.contracts.permitted("llm", operation, caller) then
				error(
					setmetatable({
						code = "denied",
						message = ("restoring session settings needs llm.%s, not allowed for the connection this runs for"):format(
							operation
						),
					}, DENIED),
					0
				)
			end
		end
	end

	--- Move the leaf (tree navigation), optionally attaching a precomputed `branch_summary`
	--- entry at `id` so the leaf moves and `session/loaded` fires once.
	local function move_leaf(caller, id, opts)
		opts = opts or {}
		local lease <close> = D.store.begin_switch()
		D.store.require_idle()
		ensure_full()
		if id ~= nil and not S.entries[id] then
			return nil, "no such session entry: " .. tostring(id)
		end
		if not opts.before_checked then
			local before =
				ctx.policy:decide("session/before-tree", table.pack({ target_id = id, current_leaf = S.leaf }))
			if not before or before.decision ~= "allow" then
				return nil, (before and before.reason) or "tree navigation cancelled"
			end
		end
		local settings = D.prepare_settings(id)
		authorize_tree_settings(caller)
		local leaf_id = id
		if opts.summary_entry then
			local entry = opts.summary_entry
			local ok, err = add(entry, nil, true)
			if not ok then
				S.entries[entry.entry_id] = nil
				return nil, "branch summary not saved: " .. tostring(err)
			end
			leaf_id = entry.entry_id
		else
			local ok, err = add({ type = "leaf", target_entry_id = id }, nil, true)
			if not ok then
				return nil, "session write failed: " .. tostring(err)
			end
		end
		local msgs = context_for(leaf_id)
		S.leaf = leaf_id
		return D.store.finish_switch(lease, function(attempt)
			attempt(function()
				ctx.agent:set_messages(msgs)
			end, true)
			attempt(function()
				D.restore_settings(leaf_id, settings)
			end, true)
			attempt(function()
				D.emit_loaded({
					messages = msgs,
					path = S.path,
					name = S.name,
					navigated = true,
					shell_cwd = S.shell_cwd,
				})
			end)
			return msgs
		end)
	end
	D.move_leaf = move_leaf

	--- Move the leaf (tree navigation). Rebuilds the agent context.
	--- set_leaf(id) -> new context | nil, err. `id` must be an entry of this session (nil: the
	--- root, before the first message); the leaf moves only once the move is written (an unknown
	--- id is rejected).
	function session.set_leaf(_, caller, id)
		return move_leaf(caller, id)
	end
	-- A marker must refer to an already observed node of this session. A broken
	-- ancestry is not a complete prefix even if an earlier node has a marker.
	local function validate_source(entries, path)
		local header = entries[1]
		if not header or type(header.session_id) ~= "string" or header.session_id == "" then
			error("Cannot fork: invalid source identity in " .. tostring(path), 0)
		end
		local seen = {}
		for index, entry in ipairs(entries) do
			maybe_yield(index)
			if index > 1 and entry.type == "session" then
				error("Cannot fork: duplicate session header in " .. tostring(path), 0)
			end
			if type(entry.entry_id) == "string" and entry.type ~= "session" then
				if seen[entry.entry_id] then
					error("Cannot fork: duplicate entry in " .. tostring(path), 0)
				end
				if type(entry.parent_entry_id) == "string" and not seen[entry.parent_entry_id] then
					error("Cannot fork: missing ancestor in " .. tostring(path), 0)
				end
				seen[entry.entry_id] = true
			end
			if entry.type == "custom" and entry.kind == "agent/run-end" then
				local d = entry.data
				if
					type(d) ~= "table"
					or d.session_id ~= header.session_id
					or type(d.run_id) ~= "string"
					or d.run_id == ""
					or type(d.leaf_entry_id) ~= "string"
					or not seen[d.leaf_entry_id]
				then
					error("Cannot fork: invalid completed turn in " .. tostring(path), 0)
				end
			end
		end
	end
	D.validate_fork_source = validate_source

	-- Build a fork entirely off the live state. The source may itself be an
	-- uncommitted read-only or damaged-session snapshot.
	local function build_snapshot_state(source, before_id, keep_all, opts)
		opts = opts or {}
		if
			opts.id
			and not (type(opts.id) == "string" and opts.id:match("^[%w%-_]+$") and #opts.id >= 8 and #opts.id <= 64)
		then
			error(("session id %q: use 8-64 letters, digits, - or _"):format(tostring(opts.id)), 0)
		end
		local source_history =
			require("session.history")({ ctx = ctx, S = source, disabled = D.disabled, store = D.store })
		local before_entry = (not keep_all and before_id) and D.resolve_item_entry(source, before_id) or nil
		local path = opts.selected_path
			or source_history.path_to(keep_all and source.leaf or (before_entry and before_entry.parent_entry_id))
		local on_path = {}
		local copied_parent
		for _, e in ipairs(path) do
			on_path[e.entry_id] = true
		end
		local snapshot = D.store.new_snapshot()
		if opts.id then
			snapshot.id = opts.id
			snapshot.planned = require("mare_lib").session_file_path(dir, opts.id)
		end
		snapshot.parent_session_id = source.id
		snapshot.restore_cwd = source.restore_cwd
		snapshot.resume_cwd = source.resume_cwd
		snapshot.shell_cwd = source.shell_cwd
		-- Prune entries keep their log position relative to the copied path entries: usage
		-- staleness (mare_lib.path_usage_stale) asks whether a prune came after the last reply.
		local lib = require("mare_lib")
		local prunes_after, covered, anchor = { [false] = {} }, {}, false
		for i, e in ipairs(source.list) do
			maybe_yield(i)
			if e.entry_id and on_path[e.entry_id] then
				anchor = e.entry_id
			elseif e.type == "prune" and type(e.target_entry_ids) == "table" then
				local targets
				for id, value in pairs(e.target_entry_ids) do
					if on_path[id] then
						targets = targets or {}
						targets[id] = value
						covered[id] = true
					end
				end
				if targets then
					local bucket = prunes_after[anchor] or {}
					prunes_after[anchor] = bucket
					bucket[#bucket + 1] = {
						type = "prune",
						target_entry_ids = targets,
						created_at_ms = e.created_at_ms or lib.to_epoch_ms(mare.task.now()),
					}
				end
			end
		end
		local entries, count, preview = {}, 0, nil
		for _, p in ipairs(prunes_after[false]) do
			entries[#entries + 1] = p
		end
		for i, e in ipairs(path) do
			maybe_yield(i)
			local source_content = e.message and e.message.content
			if type(source_content) == "table" then
				for _, block in ipairs(source_content) do
					if type(block) == "table" and block.attachment then
						error("Cannot fork safely: a source attachment was read from an unbound path", 0)
					end
				end
			end
			local c = table.copy(e)
			if opts.selected_path then
				c.parent_entry_id = copied_parent
				copied_parent = c.entry_id
				if c.type == "compaction" and c.first_kept_entry_id and not on_path[c.first_kept_entry_id] then
					c.first_kept_entry_id = nil
				end
			end
			entries[#entries + 1] = c
			for _, p in ipairs(prunes_after[e.entry_id] or {}) do
				entries[#entries + 1] = p
			end
			local m = c.type == "message" and c.message
			if m then
				count = count + 1
				if not preview and m.role == "user" and not m.compaction and not m.notification then
					for _, b in ipairs(m.content or {}) do
						if b.type == "text" then
							preview = b.text
							break
						end
					end
				end
			end
		end
		for _, e in ipairs(source.list) do
			-- A fork keeps completed-turn boundaries for every copied message, not just
			-- its last leaf, so another snapshot can still select the latest N turns.
			if e.type == "custom" and e.kind == "agent/run-end" and type(e.data) == "table" then
				local d = e.data
				if d.session_id == source.id and on_path[d.leaf_entry_id] then
					local copy = table.copy(e)
					copy.data = table.copy(d)
					copy.data.session_id = snapshot.id
					entries[#entries + 1] = copy
				end
			end
			if e.type == "custom" and e.kind == "usage/side-v1" and type(e.data) == "table" then
				local leaf_entry_id = e.data.leaf_entry_id
				if leaf_entry_id == nil or leaf_entry_id == mare.json.null or on_path[leaf_entry_id] then
					entries[#entries + 1] = table.copy(e)
				end
			end
		end
		local now = lib.to_epoch_ms(mare.task.now())
		for id, label in pairs(source.labels) do
			if on_path[id] then
				entries[#entries + 1] = { type = "label", target_entry_id = id, label = label, created_at_ms = now }
			end
		end
		-- live prune state with no prune entry behind it (compaction projections travel with
		-- their copied compaction entry) keeps the old single trailing record
		local projected = {}
		for _, e in ipairs(path) do
			if e.type == "compaction" then
				for id in pairs(e.projections or {}) do
					projected[id] = true
				end
			end
		end
		local prunes = {}
		for id, value in pairs(source.prunes) do
			if on_path[id] and not covered[id] and not projected[id] then
				prunes[id] = value
			end
		end
		if next(prunes) then
			entries[#entries + 1] = { type = "prune", target_entry_ids = prunes, created_at_ms = now }
		end
		-- Copy the entire settings history, including explicit root anchors: forks
		-- can navigate back through their own inherited turns after a restart.
		if opts.selected_path then
			local settings = source_history.settings_for(path[#path] and path[#path].entry_id)
			if settings.model then
				entries[#entries + 1] = {
					type = "model",
					model = settings.model.model,
					effort = settings.model.effort,
					at_entry_id = false,
				}
			end
		else
			for _, event in ipairs(source_history.settings_events(path[#path] and path[#path].entry_id)) do
				entries[#entries + 1] = event
			end
		end
		local name = opts.name or (source.name and (source.name .. " (fork)"))
		if name then
			entries[#entries + 1] = { type = "name", name = name }
		end
		local history = require("session.history")({ ctx = ctx, S = snapshot, disabled = D.disabled, store = D.store })
		history.apply_entries(entries)
		snapshot.msg_count, snapshot.preview = count, preview
		return snapshot, history
	end

	local function prepare_fork(source, before_id, keep_all, opts)
		local snapshot, history = build_snapshot_state(source, before_id, keep_all, opts)
		if snapshot.leaf then
			history.apply_entries({
				{
					type = "custom",
					kind = "agent/run-end",
					data = {
						session_id = snapshot.id,
						run_id = "fork",
						leaf_entry_id = snapshot.leaf,
					},
				},
			})
		end
		return snapshot, history.context_for(snapshot.leaf)
	end
	D.prepare_fork = prepare_fork

	local function completed_boundary(source, before_id, keep_all)
		local history = require("session.history")({ ctx = ctx, S = source, disabled = D.disabled, store = D.store })
		local target_entry = (not keep_all and before_id) and D.resolve_item_entry(source, before_id) or nil
		local selected = keep_all and source.leaf or (target_entry and target_entry.parent_entry_id)
		if not keep_all and before_id and not target_entry then
			require("mare_lib").fail(
				"session.unknown_item",
				("Cannot fork: item is absent from the observed source: %s"):format(tostring(before_id))
			)
		end
		local path, complete = history.path_to(selected), {}
		for index, entry in ipairs(source.list) do
			maybe_yield(index)
			local d = entry.type == "custom" and entry.kind == "agent/run-end" and entry.data
			if
				type(d) == "table"
				and d.session_id == source.id
				and type(d.run_id) == "string"
				and type(d.leaf_entry_id) == "string"
			then
				complete[d.leaf_entry_id] = index
			end
		end
		local boundary
		for i = #path, 1, -1 do
			if complete[path[i].entry_id] then
				boundary = path[i].entry_id
				break
			end
		end
		if not boundary then
			error("Cannot fork: source has no recorded completed turn", 0)
		end
		local last = complete[boundary]
		while source.list[last + 1] do
			local entry = source.list[last + 1]
			if
				D.store.is_tree_entry(entry)
				or entry.type == "leaf"
				or entry.type == "tool_state"
				or (entry.type == "custom" and (entry.kind == "agent/request" or entry.kind == "agent/step-state"))
			then
				break
			end
			last = last + 1
		end
		return boundary, last
	end
	D.completed_fork_boundary = completed_boundary

	local function completed_source(source, before_id, keep_all)
		local boundary, last = completed_boundary(source, before_id, keep_all)
		local trimmed = D.store.new_snapshot()
		trimmed.path, trimmed.resume_cwd, trimmed.shell_cwd = source.path, source.resume_cwd, source.shell_cwd
		local entries = {}
		for i = 1, last do
			entries[i] = source.list[i]
		end
		local history = require("session.history")({ ctx = ctx, S = trimmed, disabled = D.disabled, store = D.store })
		history.apply_entries(entries, true)
		-- The live shell cwd may have been changed by a discarded incomplete turn.
		-- Reconstruct it only from records in this completed observation.
		if source.shell_cwd then
			local cwd = source.resume_cwd or mare.cwd
			for _, entry in ipairs(entries) do
				if
					entry.type == "custom"
					and entry.kind == "working-directory/changed"
					and type(entry.data) == "table"
					and type(entry.data.to) == "string"
				then
					cwd = entry.data.to
				end
			end
			local st = mare.fs.stat(cwd)
			trimmed.shell_cwd = st and st.kind == "dir" and cwd or (source.resume_cwd or mare.cwd)
		end
		trimmed.restore_cwd = source.restore_cwd
		trimmed.id, trimmed.leaf = source.id, boundary
		return trimmed
	end
	D.completed_fork_source = completed_source

	-- A current, in-memory prompt still owns its original image bytes. Match them back to the
	-- exact legacy attachment name recorded by the fresh disk observation. Images hydrated from
	-- a path retain `attachment` in the live block and remain refused by prepare_fork below.
	local image_extension = {
		["image/png"] = "png",
		["image/jpeg"] = "jpg",
		["image/gif"] = "gif",
		["image/webp"] = "webp",
	}
	local function verified_attachment_name(data, mime)
		local bytes = mare.crypto.base64_decode(data)
		if not bytes then
			error("Cannot fork safely: invalid base64 image data", 0)
		end
		return mare.crypto.sha256(bytes) .. "." .. (image_extension[mime or ""] or "bin")
	end

	local function hydrate_verified_image_block(block, data, mime, mismatch_msg)
		local name = verified_attachment_name(data, mime)
		if name ~= block.attachment then
			error(mismatch_msg or "Cannot fork safely: the in-memory image no longer matches its source attachment", 0)
		end
		block.data, block.attachment = data, nil
	end

	local function read_validated_fork_snapshot(path, expected_id)
		local entries = mare.store.read_fork_snapshot(path, expected_id)
		validate_source(entries, path)
		return entries
	end
	D.read_validated_fork_snapshot = read_validated_fork_snapshot

	local function each_message_image_block(entries, fn)
		if type(entries) ~= "table" then
			return
		end
		for id, entry in pairs(entries) do
			if entry.type == "message" and entry.message and type(entry.message.content) == "table" then
				for idx, block in ipairs(entry.message.content) do
					if type(block) == "table" and block.type == "image" then
						fn(id, entry, idx, block)
					end
				end
			end
		end
	end

	function D.hydrate_verified_fork_images(source, verified_images)
		if type(verified_images) ~= "table" or not (source and source.entries) then
			return
		end
		each_message_image_block(source.entries, function(_, _, _, block)
			if block.attachment then
				local data = verified_images[block.attachment]
				if type(data) == "string" and data ~= "" then
					hydrate_verified_image_block(
						block,
						data,
						block.mime,
						"Cannot fork safely: verified attachment digest mismatch"
					)
				end
			end
		end)
	end

	local function reuse_memory_images(source)
		if S.readonly or source.id ~= S.id then
			return
		end
		each_message_image_block(source.entries, function(id, _, idx, block)
			local memory = S.entries[id]
			local cached = memory
				and memory.type == "message"
				and memory.message
				and memory.message.content
				and memory.message.content[idx]
			if
				block.attachment
				and cached
				and cached.type == "image"
				and type(cached.data) == "string"
				and cached.data ~= ""
				and not cached.attachment
			then
				hydrate_verified_image_block(block, cached.data, cached.mime)
			end
		end)
	end

	local function prepare_fork_source()
		if not S.id or not S.path then
			return nil
		end
		local drained, drain_err = D.store.drain_side(nil, S.id)
		if not drained then
			error(drain_err or "failed to drain side requests", 0)
		end
		if S.file and S.file:matches_path(S.path) then
			local async_flush = S.file.flush_async
			if type(S.file) == "userdata" and type(async_flush) == "function" and coroutine.isyieldable() then
				async_flush(S.file)
			else
				S.file:flush()
			end
		end
		local images = {}
		if not S.readonly then
			each_message_image_block(S.entries, function(_, _, _, cached)
				if type(cached.data) == "string" and cached.data ~= "" and not cached.attachment then
					local ok, name = pcall(verified_attachment_name, cached.data, cached.mime)
					if ok and name then
						images[name] = cached.data
					end
				end
			end)
		end
		return S.path, images
	end
	function session.prepare_fork(_, _)
		local path, images = prepare_fork_source()
		if not path then
			return nil
		end
		local out = { path = path }
		if type(images) == "table" and next(images) ~= nil then
			out.images = images
		end
		return out
	end

	local function fork_at(before_id, keep_all, opts, target)
		local before = ctx.policy:decide(
			"session/before-fork",
			table.pack({
				before_id = before_id,
				keep_all = keep_all,
				id = opts and opts.id or nil,
				name = opts and opts.name or nil,
			})
		)
		if not before or before.decision ~= "allow" then
			error((before and before.reason) or "session fork cancelled", 0)
		end
		local lease <close> = D.store.begin_switch()
		lease.defer_source_close = S.file
		D.store.require_idle()
		if not target and S.readonly and S.lenient then
			error("Cannot fork a damaged read-only view; back up or repair the source first", 0)
		end
		local owner = S.id
		local drained, err = D.store.drain_side(nil, owner)
		if not drained then
			error(err, 0)
		end
		-- Draining side-requests can append usage after the last semantic sync.
		-- Make those records durable before validating the prefix that the fork copies.
		if S.file and S.file:matches_path(S.path) then
			local async_flush = S.file.flush_async
			if type(S.file) == "userdata" and type(async_flush) == "function" and coroutine.isyieldable() then
				async_flush(S.file)
			else
				S.file:flush()
			end
		end
		local pre_revision = D.store.revision()
		local source = S
		if target then
			if not target.path or not mare.fs.exists(target.path) then
				require("mare_lib").fail("unknown_session", "fork target session has no persisted file")
			end
			local entries = read_validated_fork_snapshot(target.path, target.id)
			source = D.store.new_snapshot()
			source.path = target.path
			local history =
				require("session.history")({ ctx = ctx, S = source, disabled = D.disabled, store = D.store })
			history.apply_entries(entries, true)
			source.id = source.id or target.id
		elseif S.path then
			local entries = read_validated_fork_snapshot(S.path, S.id)
			source = D.store.new_snapshot()
			source.path = S.path
			local history =
				require("session.history")({ ctx = ctx, S = source, disabled = D.disabled, store = D.store })
			history.apply_entries(entries, true)
			-- An active writable session can navigate its in-memory tree. A read-only
			-- view can contain unsaved turns, so use the observed on-disk leaf.
			if not S.readonly then
				source.leaf = pre_revision.leaf
			end
			source.resume_cwd, source.shell_cwd, source.restore_cwd = S.resume_cwd, S.shell_cwd, S.restore_cwd
		end
		source = completed_source(source, before_id, keep_all)
		-- Drain running subagents only after source validation succeeds so rejected forks leave
		-- background subagents intact.
		D.store.drain_subagents()
		if S.file and S.file:matches_path(S.path) then
			S.file:flush()
		end
		local revision = D.store.revision()
		reuse_memory_images(source)
		if type(mare.args) == "table" and mare.args.fork_images ~= nil then
			mare.args.fork_images = nil
		end
		before_id, keep_all = nil, true

		local snapshot, msgs = prepare_fork(source, before_id, keep_all, opts)
		local fork_history =
			require("session.history")({ ctx = ctx, S = snapshot, disabled = D.disabled, store = D.store })
		-- Pre-check persistent PTY fence BEFORE `publish` so `PTY_POLICY_BUSY` fails cleanly
		-- without entering post-publish `critical = true` and poisoning the VM.
		D.precheck_sandbox_switch(snapshot.leaf, fork_history, snapshot, opts and opts.restore_sandbox_ceiling or nil)
		D.store.prepare_file(snapshot, lease)
		if D.ctx and D.ctx.kernel and D.ctx.kernel.begin_session_switch then
			lease.identity_switch = D.ctx.kernel:begin_session_switch({ session_id = snapshot.id })
		end
		lease.end_reason = "fork"
		D.store.publish(lease, snapshot, revision, msgs)
		return D.store.finish_switch(lease, function(attempt)
			attempt(function()
				D.restore_settings(S.leaf, nil, opts and opts.restore_sandbox_ceiling or nil)
			end, true)
			attempt(index_update)
			attempt(function()
				ctx.bus:emit("agent/messages-replaced", { messages = msgs })
			end)
			attempt(function()
				changed("fork")
			end)
			attempt(function()
				D.emit_loaded({
					messages = msgs,
					path = S.path,
					name = S.name,
					shell_cwd = S.shell_cwd,
					restore_permission_ceiling = opts and opts.restore_permission_ceiling or nil,
					restore_sandbox_ceiling = opts and opts.restore_sandbox_ceiling or nil,
				})
			end)
			return msgs
		end)
	end

	local function switch_fork(caller, opts)
		local lib = require("mare_lib")
		if type(opts) ~= "table" or type(opts.session_id) ~= "string" or opts.session_id == "" then
			lib.fail("invalid_arguments", "session.fork requires { session_id = string }")
		end
		if opts.before_item_id ~= nil and (type(opts.before_item_id) ~= "string" or opts.before_item_id == "") then
			lib.fail("invalid_arguments", "before_item_id must be a non-empty string")
		end
		if opts.name ~= nil and type(opts.name) ~= "string" then
			lib.fail("invalid_arguments", "name must be a string")
		end
		if opts.new_session_id ~= nil and (type(opts.new_session_id) ~= "string" or opts.new_session_id == "") then
			lib.fail("invalid_arguments", "new_session_id must be a non-empty string")
		end
		local target
		if opts.session_id ~= S.id then
			local rerr, err_kind
			target, rerr, err_kind = D.metadata.resolve_any(opts.session_id, caller)
			if not target then
				lib.fail(err_kind or "unknown_session", rerr or ("no session matches %q"):format(opts.session_id))
			end
			if target.id == S.id then
				target = nil
			end
		end
		local ok, err = pcall(fork_at, opts.before_item_id, opts.before_item_id == nil, {
			id = opts.new_session_id,
			name = opts.name ~= "" and opts.name or nil,
			caller = caller,
			restore_permission_ceiling = opts.restore_permission_ceiling,
			restore_sandbox_ceiling = opts.restore_sandbox_ceiling,
		}, target)
		if not ok then
			if type(err) == "table" and type(err.code) == "string" then
				error(err, 0)
			end
			local msg = tostring(err)
			if
				msg:find("turn is running", 1, true)
				or msg:find("switching", 1, true)
				or msg:find("session locked", 1, true)
				or msg:find("still stopping", 1, true)
				or msg:find("did not stop", 1, true)
				or msg:find("already open in this process", 1, true)
			then
				lib.fail("busy", msg)
			elseif
				msg:find("session file is corrupt", 1, true)
				or msg:find("damaged", 1, true)
				or msg:find("malformed", 1, true)
			then
				lib.fail("corrupt", msg)
			elseif msg:find("session fork cancelled", 1, true) then
				lib.fail("cancelled", msg)
			elseif msg:find("no recorded completed turn", 1, true) or msg:find("Cannot fork", 1, true) then
				lib.fail("invalid_arguments", msg)
			else
				lib.fail("persist_failed", msg)
			end
		end
		return D.current_session_snapshot()
	end

	return { fork_at = fork_at, switch_fork = switch_fork, build_snapshot_state = build_snapshot_state }
end
