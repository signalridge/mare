-- session.fork_snapshot: non-switching, non-blocking SES-FORK primitive (`session.snapshot` / `session.snapshot_context`).
-- Reads committed state under the locked writer (or fork snapshot for inactive sessions), synthesizes
-- `prepared` / `started` (`outcome_unknown` / `interrupted`) tool call outcomes, closes mid-turn prefixes
-- with the existing `<turn_aborted>` marker, copies attachments independently, and never mutates or
-- cancels the source session.
return function(D)
	local ctx, S, session = D.ctx, D.S, D.session
	local lib = require("mare_lib")

	local INTERRUPT_MARKER =
		"<turn_aborted>The snapshot ended during an unfinished turn; the source session may continue independently.</turn_aborted>"

	local function build_model_override(source, source_history, boundary_id, opts)
		if opts.model == nil and opts.effort == nil then
			return nil
		end
		if opts.model ~= nil and (type(opts.model) ~= "string" or opts.model == "") then
			lib.fail("invalid_arguments", "model must be a non-empty string")
		end
		if opts.effort ~= nil and (type(opts.effort) ~= "string" or opts.effort == "") then
			lib.fail("invalid_arguments", "effort must be a non-empty string")
		end
		local llm = D.follow.llm and D.follow.llm() or nil
		local model_ref = opts.model
		if model_ref and llm and lib.callable(llm.resolve) then
			local resolved = llm:resolve(model_ref)
			if not resolved then
				lib.fail("invalid_arguments", ("unknown model: %s"):format(model_ref))
			end
			model_ref = resolved.ref or model_ref
		end
		local inherited = source_history.settings_for(boundary_id)
		if not model_ref then
			model_ref = inherited.model and inherited.model.model or nil
			if not model_ref then
				local path = source_history.path_to(boundary_id)
				for i = #path, 1, -1 do
					local m = path[i].type == "message" and path[i].message
					if m and m.role == "assistant" and m.model_id and m.provider_id then
						model_ref = m.provider_id .. "/" .. m.model_id
						break
					end
				end
			end
			if not model_ref and llm and lib.callable(llm.current) then
				local cur = llm:current()
				model_ref = cur and cur.ref or nil
			end
		end
		local effort = opts.effort or (inherited.model and inherited.model.effort) or nil
		if model_ref == nil and effort == nil then
			return nil
		end
		return { model = model_ref, effort = effort }
	end

	local function hydrate_fork_attachments(source, path)
		if not source.path then
			return
		end
		local max_bytes = 32 * 1024 * 1024
		for _, entry in ipairs(path) do
			local m = entry.type == "message" and entry.message
			if m and type(m.content) == "table" then
				for _, b in ipairs(m.content) do
					if type(b) == "table" and b.type == "image" and b.attachment and not b.data then
						local ok, bytes = pcall(mare.store.read_attachment, source.path, b.attachment, max_bytes)
						if not ok or type(bytes) ~= "string" then
							error("Cannot fork safely: missing or invalid attachment " .. tostring(b.attachment), 0)
						end
						b.data = mare.crypto.base64_encode(bytes)
						b.attachment = nil
					end
				end
			end
		end
	end

	local function tool_side_effect(source, call)
		local st = source.tool_states and source.tool_states[call.id]
		local se = st and st.side_effect
		if not se then
			local tools = ctx:get("tools")
			if tools and lib.callable(tools.side_effect) then
				local ok, val = pcall(tools.side_effect, tools, call.name)
				if ok and type(val) == "string" then
					se = val
				end
			end
		end
		return st, se or "external"
	end

	local function synthetic_result_text(st, se)
		if st and st.state == "prepared" then
			return "interrupted: this call never started; it is safe to run it again if still needed."
		end
		if (se or "external") ~= "read" then
			return "outcome_unknown: this call may or may not have completed, and processes it started may still be running; check the actual state before re-running it. (synthesized at fork boundary; source session may still be executing, side effects unknown — do not auto-replay)"
		end
		return "interrupted: this call did not finish; it is safe to run it again."
	end

	local function collect_run_ends(source)
		local run_ends = {}
		for i, entry in ipairs(source.list or {}) do
			lib.maybe_yield(i)
			local d = entry.type == "custom" and entry.kind == "agent/run-end" and entry.data
			if type(d) == "table" and d.session_id == source.id and type(d.leaf_entry_id) == "string" then
				run_ends[d.leaf_entry_id] = true
			end
		end
		return run_ends
	end

	local function build_mid_turn_closure(source, path, boundary_id, initiating_tool_call_id)
		local run_ends = collect_run_ends(source)
		local have = {}
		for _, entry in ipairs(path) do
			local m = entry.type == "message" and entry.message
			if m and m.role == "tool_result" and m.tool_call_id then
				have[m.tool_call_id] = true
			end
		end
		local k = #path
		while k >= 1 do
			local m = path[k].type == "message" and path[k].message
			if not (m and m.role == "tool_result") then
				break
			end
			k = k - 1
		end
		local synthesized, found_initiator = {}, false
		local tail_entry = k >= 1 and path[k] or nil
		local tail_msg = tail_entry and tail_entry.type == "message" and tail_entry.message
		if tail_msg and tail_msg.role == "assistant" then
			for _, b in ipairs(tail_msg.content or {}) do
				if type(b) == "table" and b.type == "tool_call" and not b.hosted and b.id and not have[b.id] then
					local st, se = tool_side_effect(source, b)
					local initiating = initiating_tool_call_id ~= nil and b.id == initiating_tool_call_id
					found_initiator = found_initiator or initiating
					synthesized[#synthesized + 1] = {
						call = b,
						state = st and st.state or "started",
						side_effect = se,
						text = initiating
								and "The parent is still executing this tool call; its result is pending in the parent session."
							or synthetic_result_text(st, se),
						initiating = initiating,
					}
				end
			end
		end
		if initiating_tool_call_id and not found_initiator then
			lib.fail(
				"invalid_arguments",
				"initiating_tool_call_id must name a pending tool call at the snapshot boundary"
			)
		end
		local last_entry = path[#path]
		local last_msg = last_entry and last_entry.type == "message" and last_entry.message
		local is_mid_turn = #synthesized > 0
			or (
				last_msg ~= nil
				and (
					(last_msg.role == "user" and not last_msg.compaction and not last_msg.turn_aborted)
					or last_msg.role == "tool_result"
					or (last_msg.role == "assistant" and not run_ends[boundary_id])
				)
			)
		return synthesized, is_mid_turn
	end

	-- Completed turns are marked by durable run-end records on the selected ancestry.
	local function select_completed_turns(source, path, count, include_current)
		if not count then
			return path
		end
		local ends = collect_run_ends(source)
		local turns, first = {}, nil
		for i, entry in ipairs(path) do
			local message = entry.type == "message" and entry.message
			if
				message and (message.turn_aborted or (message.role == "assistant" and message.stop_reason == "error"))
			then
				first = nil
			elseif
				message
				and message.role == "user"
				and not message.notification
				and not message.compaction
				and (not first or not entry.input_id)
			then
				first = i
			end
			if first and ends[entry.entry_id] then
				turns[#turns + 1] = { first, i }
				first = nil
			end
		end
		local selected = {}
		for i = math.max(1, #turns - count + 1), #turns do
			for j = turns[i][1], turns[i][2] do
				selected[#selected + 1] = path[j]
			end
		end
		if include_current then
			if not first then
				lib.fail("invalid_arguments", "initiating_tool_call_id requires an unfinished parent turn")
			end
			for i = first, #path do
				selected[#selected + 1] = path[i]
			end
		end
		return selected
	end

	local function validate_before_item_id(before_item_id)
		if type(before_item_id) ~= "string" or before_item_id == "" then
			lib.fail("invalid_arguments", "before_item_id must be a non-empty string")
		end
	end

	local function load_source(opts, caller, allow_memory)
		local sid = opts.session_id or S.id
		if type(sid) ~= "string" or sid == "" then
			lib.fail("invalid_arguments", "session_id must be a non-empty string")
		end
		local target
		if sid ~= S.id then
			local rerr, err_kind
			target, rerr, err_kind = D.metadata.resolve_any(sid, caller)
			if not target then
				lib.fail(err_kind or "unknown_session", rerr or ("no session matches %q"):format(sid))
			end
			if target.id == S.id then
				target = nil
			end
		end
		local entries, source_path, expected_id
		if target then
			if not target.path or not mare.fs.exists(target.path) then
				lib.fail("session.no_history", "target session has no persisted history")
			end
			source_path = target.path
			expected_id = target.id
			entries = mare.store.read_fork_snapshot(target.path, target.id)
		else
			if S.readonly and S.lenient then
				lib.fail(
					"invalid_arguments",
					"Cannot snapshot a damaged read-only view; back up or repair the source first"
				)
			end
			if S.file then
				if S.file.flush then
					pcall(S.file.flush, S.file)
				end
				source_path = S.path or S.planned
				expected_id = S.id
				entries = mare.store.read_snapshot(S.file)
			elseif S.path and mare.fs.exists(S.path) then
				source_path = S.path
				expected_id = S.id
				entries = mare.store.read_fork_snapshot(S.path, S.id)
			elseif D.recorded_path then
				source_path = D.recorded_path
				expected_id = S.id
				entries = mare.store.read_fork_snapshot(source_path, S.id)
			elseif allow_memory then
				local msgs = ctx.agent and ctx.agent:messages() or {}
				if opts.before_item_id ~= nil and #msgs == 0 then
					validate_before_item_id(opts.before_item_id)
					lib.fail(
						"session.unknown_item",
						("item is absent from the observed source: %s"):format(opts.before_item_id)
					)
				end
				local mem = D.store.new_snapshot()
				mem.id = S.id
				mem.name = S.name
				mem.cwd = S.cwd or mare.cwd
				mem.leaf = S.leaf
				return mem, require("session.history")({ ctx = ctx, S = mem, disabled = true, store = D.store }), true
			else
				lib.fail("session.no_history", "source session has no persisted history")
			end
		end
		D.validate_fork_source(entries, source_path)
		local source = D.store.new_snapshot()
		source.path = source_path
		source.cwd = S.cwd or mare.cwd
		local history = require("session.history")({ ctx = ctx, S = source, disabled = D.disabled, store = D.store })
		history.apply_entries(entries, true)
		source.id = source.id or expected_id
		if not target then
			source.resume_cwd, source.shell_cwd, source.restore_cwd = S.resume_cwd, S.shell_cwd, S.restore_cwd
		end
		if not source.cwd and entries[1] and type(entries[1].cwd) == "string" then
			source.cwd = entries[1].cwd
		end
		return source, history, false
	end

	local function resolve_boundary(source, before_item_id)
		if before_item_id ~= nil then
			validate_before_item_id(before_item_id)
			local target_entry = D.resolve_item_entry(source, before_item_id)
			if not target_entry then
				lib.fail("session.unknown_item", ("item is absent from the observed source: %s"):format(before_item_id))
			end
			return target_entry.parent_entry_id
		end
		if not source.leaf then
			lib.fail("session.no_history", "source session has no persisted history")
		end
		return source.leaf
	end

	--- Build a non-switching, non-blocking snapshot context (`session:snapshot_context`).
	function session.snapshot_context(_, caller, opts)
		opts = opts or {}
		local source, source_history, memory_only = load_source(opts, caller, opts.allow_memory == true)
		if memory_only then
			local msgs = {}
			for _, m in ipairs(ctx.agent:messages() or {}) do
				msgs[#msgs + 1] = lib.copy_deep(m)
			end
			local llm = D.follow.llm and D.follow.llm() or nil
			local cur = llm and lib.callable(llm.current) and llm:current() or nil
			local params = llm and cur and lib.callable(llm.resolve_params) and llm:resolve_params(cur).params or {}
			return {
				session_id = source.id,
				name = source.name,
				cwd = source.cwd or mare.cwd,
				leaf_entry_id = source.leaf,
				messages = mare.json.array(msgs),
				model = opts.model or (cur and cur.ref or nil),
				effort = opts.effort or (params and params.effort or nil),
				persisted = false,
				source_count = #msgs,
			}
		end
		-- A persistent fork can validly contain no selected turns.  Its parent link
		-- or `allow_memory = true` distinguishes an empty side-chat snapshot from a history-requiring fork.
		local boundary_id
		if source.leaf or opts.before_item_id or (not source.parent_session_id and not opts.allow_memory) then
			boundary_id = resolve_boundary(source, opts.before_item_id)
		end
		local path = source_history.path_to(boundary_id)
		hydrate_fork_attachments(source, path)
		local synthesized, is_mid_turn = build_mid_turn_closure(source, path, boundary_id)
		local inherited = source_history.settings_for(boundary_id)
		local msgs = source_history.context_for(boundary_id)
		local completed = collect_run_ends(source)
		local completed_turn_indices = mare.json.array()
		for i = 1, #msgs do
			lib.maybe_yield(i, 1024)
			if completed[source.ids[msgs[i]]] then
				completed_turn_indices[#completed_turn_indices + 1] = i
			end
			msgs[i] = lib.copy_deep(msgs[i])
		end
		local now = mare.task.now()
		for _, s in ipairs(synthesized) do
			msgs[#msgs + 1] = {
				role = "tool_result",
				tool_call_id = s.call.id,
				tool_name = s.call.name,
				content = mare.json.array({ { type = "text", text = s.text } }),
				is_error = not s.initiating,
				timestamp = now,
			}
		end
		if is_mid_turn then
			msgs[#msgs + 1] = {
				role = "user",
				notification = true,
				turn_aborted = true,
				content = mare.json.array({ { type = "text", text = INTERRUPT_MARKER } }),
				timestamp = now,
			}
		end
		local override = build_model_override(source, source_history, boundary_id, opts)
		local model_ref = (override and override.model) or (inherited.model and inherited.model.model) or nil
		local effort = (override and override.effort) or (inherited.model and inherited.model.effort) or nil
		if not model_ref then
			for i = #path, 1, -1 do
				local m = path[i].type == "message" and path[i].message
				if m and m.role == "assistant" and m.model_id and m.provider_id then
					model_ref = m.provider_id .. "/" .. m.model_id
					break
				end
			end
		end
		return {
			session_id = source.id,
			name = source.name,
			cwd = source.cwd or mare.cwd,
			path = source.path,
			leaf_entry_id = boundary_id,
			messages = msgs,
			completed_turn_indices = completed_turn_indices,
			model = model_ref,
			effort = effort,
			persisted = true,
			source_count = #msgs,
		}
	end

	--- Persist a new session snapshot file without switching S or cancelling the source turn.
	local function create_snapshot(opts, caller)
		opts = opts or {}
		local before = ctx.policy:decide(
			"session/before-fork",
			table.pack({
				before_id = opts.before_item_id,
				keep_all = opts.before_item_id == nil,
				id = opts.id,
				name = opts.name,
			})
		)
		if not before or before.decision ~= "allow" then
			lib.fail("denied", (before and before.reason) or "session snapshot cancelled")
		end
		local source, source_history = load_source(opts, caller)
		local boundary_id = resolve_boundary(source, opts.before_item_id)
		local path = source_history.path_to(boundary_id)
		local source_leaf_entry_id = boundary_id
		path = select_completed_turns(source, path, opts.last_completed_turns, opts.initiating_tool_call_id ~= nil)
		local selected_leaf = path[#path] and path[#path].entry_id or nil
		local override = build_model_override(source, source_history, boundary_id, opts)
		hydrate_fork_attachments(source, path)
		local synthesized, is_mid_turn =
			build_mid_turn_closure(source, path, selected_leaf, opts.initiating_tool_call_id)

		local snapshot = D.fork.build_snapshot_state(source, opts.before_item_id, opts.before_item_id == nil, {
			id = opts.id,
			name = opts.name ~= "" and opts.name or nil,
			selected_path = opts.last_completed_turns and path or nil,
		})
		local extra = {}
		local now = lib.to_epoch_ms(mare.task.now())
		local parent = snapshot.leaf
		local closure_id
		for _, s in ipairs(synthesized) do
			local result_id = D.store.new_id()
			extra[#extra + 1] = {
				type = "message",
				entry_id = result_id,
				parent_entry_id = parent,
				created_at_ms = now,
				message = {
					role = "tool_result",
					tool_call_id = s.call.id,
					tool_name = s.call.name,
					content = mare.json.array({ { type = "text", text = s.text } }),
					is_error = not s.initiating,
					timestamp = now,
				},
			}
			extra[#extra + 1] = {
				type = "custom",
				kind = "session/fork-boundary",
				provenance = "session-host",
				at_entry_id = result_id,
				created_at_ms = now,
				data = {
					source_session_id = source.id,
					tool_call_id = s.call.id,
					state = s.state,
					side_effect = s.side_effect,
				},
			}
			parent = result_id
			closure_id = result_id
		end
		if is_mid_turn then
			local aborted_id = D.store.new_id()
			extra[#extra + 1] = {
				type = "message",
				entry_id = aborted_id,
				parent_entry_id = parent,
				created_at_ms = now,
				message = {
					role = "user",
					notification = true,
					turn_aborted = true,
					content = mare.json.array({ { type = "text", text = INTERRUPT_MARKER } }),
					timestamp = now,
				},
			}
			closure_id = aborted_id
		end
		local hist = require("session.history")({ ctx = ctx, S = snapshot, disabled = D.disabled, store = D.store })
		if #extra > 0 then
			hist.apply_entries(extra)
		end
		if snapshot.leaf then
			hist.apply_entries({
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
		if override then
			hist.apply_entries({
				{
					type = "model",
					model = override.model,
					effort = override.effort,
					at_entry_id = snapshot.leaf or false,
				},
			})
		end

		local lease = { transferred = false }
		local stage_guard <close> = setmetatable({}, {
			__close = function()
				if not lease.transferred then
					if lease.stage then
						pcall(lease.stage.close, lease.stage)
					end
					if lease.target then
						pcall(lease.target.discard or lease.target.close, lease.target)
					end
				end
			end,
		})
		snapshot.cwd = source.cwd or mare.cwd
		snapshot.planned = require("mare_lib").session_file_path(mare.store.dir(snapshot.cwd), snapshot.id)
		D.store.prepare_file(snapshot, lease, { auxiliary = true })
		local f = lease.target
		if not f then
			lib.fail("unsupported", "session snapshot requires persistence")
		end
		local cok, cerr = pcall(
			f.append,
			f,
			{ type = "state", state = "closed", created_at_ms = lib.to_epoch_ms(mare.task.now()) },
			{ sync = true }
		)
		local clok, clerr = pcall(f.close, f)
		lease.transferred = true
		lease.target = nil
		if not cok or not clok then
			lib.fail("persist_failed", "snapshot file close failed: " .. tostring(cok and clerr or cerr))
		end
		pcall(mare.store.index_append, { session_id = snapshot.id, path = snapshot.path, cwd = snapshot.cwd })
		return D.session_snapshot({
			session_id = snapshot.id,
			name = snapshot.name,
			name_revision = snapshot.name_revision or 0,
			cwd = snapshot.cwd,
			path = snapshot.path,
			parent_session_id = snapshot.parent_session_id,
			source_leaf_entry_id = source_leaf_entry_id,
			leaf_entry_id = snapshot.leaf,
			loaded = false,
			ephemeral = false,
			archived = false,
			created_at_ms = snapshot.created_at_ms,
			updated_at_ms = mare.task.now(),
		})
	end

	local MAX_SNAPSHOT_RECEIPTS = 256
	local snapshot_receipts = {}
	local snapshot_receipt_order = {}
	local function snapshot_receipt_scope(caller)
		if mare.contracts and mare.contracts.is_peer(caller) then
			return "peer:" .. tostring(lib.rpc_caller_id(caller) or "anon")
		end
		return "primary"
	end
	local function prune_deleted_snapshot_receipts(deleted_id)
		if type(deleted_id) ~= "string" or deleted_id == "" then
			return
		end
		for k, entry in pairs(snapshot_receipts) do
			if type(entry.snapshot) == "table" and entry.snapshot.session_id == deleted_id then
				snapshot_receipts[k] = nil
			end
		end
		local kept = {}
		for _, k in ipairs(snapshot_receipt_order) do
			if snapshot_receipts[k] ~= nil then
				kept[#kept + 1] = k
			end
		end
		snapshot_receipt_order = kept
	end
	if ctx and ctx.on then
		ctx:on("session/deleted", function(ev)
			if type(ev) == "table" then
				prune_deleted_snapshot_receipts(ev.session_id)
			end
		end)
	end
	local function store_snapshot_receipt(key, fp, snapshot)
		if snapshot_receipts[key] == nil then
			snapshot_receipt_order[#snapshot_receipt_order + 1] = key
		end
		snapshot_receipts[key] = { fp = fp, snapshot = snapshot }
		while #snapshot_receipt_order > MAX_SNAPSHOT_RECEIPTS do
			local oldest = table.remove(snapshot_receipt_order, 1)
			snapshot_receipts[oldest] = nil
		end
	end
	function session.snapshot(_, caller, opts)
		if type(opts) ~= "table" or type(opts.session_id) ~= "string" or opts.session_id == "" then
			lib.fail("invalid_arguments", "session.snapshot requires { session_id = string }")
		end
		if opts.before_item_id ~= nil and (type(opts.before_item_id) ~= "string" or opts.before_item_id == "") then
			lib.fail("invalid_arguments", "before_item_id must be a non-empty string")
		end
		if opts.name ~= nil and type(opts.name) ~= "string" then
			lib.fail("invalid_arguments", "name must be a string")
		end
		if opts.model ~= nil and (type(opts.model) ~= "string" or opts.model == "") then
			lib.fail("invalid_arguments", "model must be a non-empty string")
		end
		if opts.effort ~= nil and (type(opts.effort) ~= "string" or opts.effort == "") then
			lib.fail("invalid_arguments", "effort must be a non-empty string")
		end
		D.check_expected_epoch(opts.expected_epoch, opts.session_id)
		if
			opts.last_completed_turns ~= nil
			and (math.type(opts.last_completed_turns) ~= "integer" or opts.last_completed_turns < 1)
		then
			lib.fail("invalid_arguments", "last_completed_turns must be a positive integer")
		end
		if
			opts.initiating_tool_call_id ~= nil
			and (type(opts.initiating_tool_call_id) ~= "string" or opts.initiating_tool_call_id == "")
		then
			lib.fail("invalid_arguments", "initiating_tool_call_id must be a non-empty string")
		end
		local req_id = opts.request_id
		local req_key, req_fp
		if req_id ~= nil then
			if type(req_id) ~= "string" or req_id == "" then
				lib.fail("invalid_arguments", "request_id must be a non-empty string")
			end
			req_key = snapshot_receipt_scope(caller) .. "\0" .. req_id
			req_fp = mare.json.encode_strict({
				session_id = opts.session_id,
				before_item_id = opts.before_item_id or false,
				name = opts.name or false,
				model = opts.model or false,
				effort = opts.effort or false,
				last_completed_turns = opts.last_completed_turns or false,
				initiating_tool_call_id = opts.initiating_tool_call_id or false,
			})
			local prev = snapshot_receipts[req_key]
			if prev then
				local path = type(prev.snapshot) == "table" and prev.snapshot.path or nil
				if type(path) == "string" and path ~= "" and not mare.fs.exists(path) then
					snapshot_receipts[req_key] = nil
				else
					if prev.fp ~= req_fp then
						lib.fail("request_id_conflict", "request_id was reused with different parameters")
					end
					return prev.snapshot
				end
			end
		end
		local ok, res = pcall(create_snapshot, opts, caller)
		if not ok then
			if type(res) == "table" and type(res.code) == "string" then
				error(res, 0)
			end
			local msg = tostring(res)
			if msg:find("no persisted history", 1, true) then
				lib.fail("session.no_history", msg)
			else
				lib.fail("invalid_arguments", msg)
			end
		end
		if req_key ~= nil then
			store_snapshot_receipt(req_key, req_fp, res)
		end
		return res
	end

	return {
		create_snapshot = create_snapshot,
	}
end
