-- session.recorder: what the agent and plugins write into the session (name, plugin
-- records, tool states, messages, request markers, queued inputs);
-- part of the `session` plugin (plugins/session.lua), loaded at the start of its apply.
return function(D)
	local ctx, S, session = D.ctx, D.S, D.session
	local new_id, add = D.store.new_id, D.store.add
	local index_update = D.history.index_update

	--- D.set_name(name, expected_revision) -> true | nil, err, err_code. Written before it takes
	--- effect; a session without a file yet keeps it for the first write (naming alone never creates
	--- a session). When `expected_revision` is given, it is checked and bumped under the commit lock.
	function D.set_name(name, expected_revision)
		name = mare.projection.project(name)
		local check_in_lock
		if expected_revision ~= nil then
			check_in_lock = function()
				local current_rev = S.name_revision or 0
				if current_rev ~= expected_revision then
					return false,
						("session revision %d does not match expected_revision %d"):format(
							current_rev,
							expected_revision
						),
						"session.revision_conflict"
				end
				return true
			end
		end
		local ok, err, _, _, err_code = add({ type = "name", name = name }, S.file ~= nil, false, nil, function()
			S.name = name
			S.name_revision = (S.name_revision or 0) + 1
		end, { check_in_lock = check_in_lock })
		if not ok then
			if err_code == "session.revision_conflict" then
				return nil, tostring(err), err_code
			end
			return nil, "session write failed: " .. tostring(err), err_code
		end
		index_update()
		ctx.bus:emit("session/named", { name = name })
		return true
	end
	local TRUSTED_FILTERED_KINDS = {
		["permission/preset"] = true,
		["sandbox/mode"] = true,
		["session/transition-initial"] = true,
	}

	function session.add_record(_, caller, kind, data, opts)
		local lib = require("mare_lib")
		if type(kind) ~= "string" or kind == "" then
			lib.fail("invalid_arguments", "record kind must be a non-empty string")
		end
		local is_remote = lib.rpc_caller_id(caller) ~= nil or (mare.contracts and mare.contracts.is_peer(caller))
		if D.store.is_reserved_record_kind(kind) and is_remote then
			lib.fail("invalid_arguments", "session.add_record cannot write reserved internal kind: " .. kind)
		end
		if opts ~= nil and type(opts) ~= "table" then
			lib.fail("invalid_arguments", "session.add_record options must be a table")
		end
		opts = opts or {}
		local ext_id
		for key, value in pairs(opts) do
			if key == "session_id" or key == "session_epoch" then
				if type(value) ~= "string" then
					lib.fail("invalid_arguments", "session identity must be strings")
				end
			elseif key == "entry_id" then
				ext_id = value
			elseif (key ~= "persist" and key ~= "sync") or type(value) ~= "boolean" then
				lib.fail(
					"invalid_arguments",
					"session.add_record options accept boolean persist/sync and string entry_id/session_id/session_epoch"
				)
			end
		end
		D.store.check_new_id(ext_id, true)
		local owner = D.store.check_owner(opts, true)
		local record_id = ext_id or D.store.new_id()
		-- approval records pass the unified redactor before they're written
		if kind:find("^permission/") or kind:find("^approval/") then
			data = mare.text.redact(data)
		end
		data = D.store.copy_from_tool(data)
		local sync = opts.sync == true
		if sync and (D.disabled or S.readonly or S.unstable) then
			lib.fail(
				"persist_failed",
				"durable session writes are unavailable (disabled, read-only or unstable session)"
			)
		end
		local persist = sync or S.file ~= nil or opts.persist == true
		local committed_entry
		local ok, err = D.store.run_host(function(valid)
			if sync and (D.disabled or S.readonly or S.unstable) then
				return nil, "durable session writes are unavailable (disabled, read-only or unstable session)"
			end
			local e = {
				type = "custom",
				kind = kind,
				data = data,
				entry_id = record_id,
				at_entry_id = S.leaf or false,
				created_at_ms = lib.to_epoch_ms(mare.task.now()),
			}
			if D.store.is_reserved_record_kind(kind) then
				e.provenance = "session-host"
			end
			committed_entry = e
			return add(e, persist, sync, valid)
		end, owner)
		if not ok then
			lib.fail("persist_failed", tostring(err or "session record write failed"))
		end
		if not S.preview then
			ctx.bus:emit("session/recorded", { entry = committed_entry })
		end
		return { record_id = record_id, durable = sync }
	end
	--- Commit model-step state and already validated disclosure messages atomically.
	function session.commit_step(_, _, record, messages, opts)
		if type(opts) ~= "table" or not mare.task.is_token(opts.cancel) then
			return nil, "step commit requires cancellation and session ownership"
		end
		for key in pairs(opts) do
			if key ~= "cancel" and key ~= "session_id" and key ~= "session_epoch" and key ~= "persist" then
				return nil, "unknown step commit option"
			end
		end
		if opts.session_id ~= S.id or opts.session_epoch ~= D.store.current_epoch() then
			return nil, "stale step commit owner"
		end
		if type(opts.persist) ~= "boolean" then
			return nil, "step persist must be boolean"
		end
		if D.disabled and opts.persist then
			return nil, "durable session disabled"
		end
		if S.readonly and opts.persist then
			return nil, "session is read-only"
		end
		local original_messages = messages
		local valid, captured = pcall(mare.activity.capture, function()
			assert(
				type(record) == "table"
					and record.version == 1
					and type(record.step_id) == "string"
					and type(record.turn_id) == "string"
					and type(record.step) == "number"
					and record.step >= 1
					and record.step % 1 == 0
					and type(record.states) == "table",
				"invalid step record"
			)
			for key in pairs(record) do
				assert(
					key == "version" or key == "step_id" or key == "turn_id" or key == "step" or key == "states",
					"unknown step record field"
				)
			end
			assert(type(messages) == "table" and #messages <= 128, "invalid disclosure batch")
			for key, message in pairs(messages) do
				assert(
					type(key) == "number" and key >= 1 and key <= #messages and key % 1 == 0,
					"disclosures must be an array"
				)
				assert(
					type(message) == "table" and message.role == "user" and message.notification == true,
					"invalid disclosure message"
				)
				local source = message.context_disclosure
				assert(
					type(source) == "table"
						and type(source.namespace) == "string"
						and #source.namespace <= 128
						and type(source.source) == "string"
						and #source.source <= 128
						and source.step_id == record.step_id
						and source.state ~= nil
						and message.input_source == source.source,
					"invalid disclosure source"
				)
				assert(type(message.content) == "table" and #message.content >= 1, "invalid disclosure content")
				for _, block in ipairs(message.content) do
					assert(
						type(block) == "table" and block.type == "text" and type(block.text) == "string",
						"disclosure content must be text blocks"
					)
				end
			end
			return { record = record, messages = messages }
		end)
		if not valid then
			return nil, tostring(captured)
		end
		record, messages = captured.record, captured.messages
		local ok_proj, proj_err = pcall(function()
			record = mare.projection.project(record, { preserve_invalid = true })
			for i, message in ipairs(messages) do
				messages[i] = mare.projection.project(message, { preserve_invalid = true })
			end
		end)
		if not ok_proj then
			return nil, tostring(proj_err)
		end
		local owner, generation = S.id, S.generation
		return D.store.run_host(function(active)
			local now_ms = require("mare_lib").to_epoch_ms(mare.task.now())
			local entries, ids, next_leaf = {}, {}, S.leaf
			for i, message in ipairs(messages) do
				local id = new_id()
				ids[i] = id
				entries[#entries + 1] = {
					type = "message",
					entry_id = id,
					parent_entry_id = next_leaf,
					message = message,
					created_at_ms = now_ms,
				}
				next_leaf = id
			end
			entries[#entries + 1] = {
				type = "custom",
				kind = "agent/step-state",
				data = record,
				created_at_ms = now_ms,
			}
			local function check()
				return (not active or active())
					and S.id == owner
					and S.generation == generation
					and not opts.cancel:is_cancelled()
			end
			local ok, err = D.store.add_batch(entries, check, function()
				if #messages > 0 then
					S.leaf = next_leaf
				end
				for i, message in ipairs(messages) do
					S.ids[message] = ids[i]
					if original_messages[i] then
						S.ids[original_messages[i]] = ids[i]
					end
				end
				S.msg_count = S.msg_count + #messages
			end, {
				persist = opts.persist,
				sync = opts.persist,
				allow_readonly = not opts.persist,
				bind_leaf = function(es, st)
					local leaf = S.leaf
					local cur = leaf
					for i = 1, #messages do
						es[i].parent_entry_id = cur
						st[i].parent_entry_id = cur
						cur = ids[i]
					end
					next_leaf = cur
					return function()
						return #messages == 0 or S.leaf == leaf
					end
				end,
			})
			if not ok then
				return nil, "step commit failed: " .. tostring(err)
			end
			for i, message in ipairs(messages) do
				original_messages[i] = message
			end
			return true
		end, { id = owner, generation = generation })
	end
	function session.list_records(_, _, kind, opts)
		local lib = require("mare_lib")
		if type(kind) ~= "string" or kind == "" then
			lib.fail("invalid_arguments", "record kind must be a non-empty string")
		end
		if opts ~= nil and type(opts) ~= "table" then
			lib.fail("invalid_arguments", "session.list_records options must be a table")
		end
		opts = opts or {}
		local branch = opts.branch or nil
		if branch ~= nil and branch ~= "current" and branch ~= "all" then
			lib.fail("invalid_arguments", "branch must be `current` or `all`")
		end
		if opts.session_id ~= nil or opts.session_epoch ~= nil then
			if
				(opts.session_id ~= nil and type(opts.session_id) ~= "string")
				or (opts.session_epoch ~= nil and type(opts.session_epoch) ~= "string")
			then
				lib.fail("invalid_arguments", "session identity must be strings")
			end
			D.store.check_owner(opts, true)
		end
		local current = branch == "current"
		local use_pre_tail = S.partial and (current or opts.loaded_only ~= true) and S.pre_tail_custom ~= nil
		if (current or opts.loaded_only ~= true) and not use_pre_tail then
			D.history.ensure_full()
		end
		local rows = mare.json.array({})
		local function collect(entries)
			for _, e in ipairs(entries) do
				if e.type == "custom" and e.kind == kind then
					if not TRUSTED_FILTERED_KINDS[kind] or e.provenance == "session-host" then
						rows[#rows + 1] = e.data
					end
				end
			end
		end
		if use_pre_tail then
			collect(current and (S.pre_tail_branch_custom or {}) or S.pre_tail_custom)
		end
		collect(current and D.history.events_for(S.leaf) or S.list)
		local projected = mare.projection.project(rows, { preserve_invalid = true })
		return D.page("session.list_records", opts, projected, { kind = kind, branch = branch or "all" })
	end
	-- persist every agent message on the current branch
	-- persistence barrier: the agent asks before acting on a message (model
	-- request / tool run) and stops when the write fails — notifications can't fail
	local lib = require("mare_lib")
	local function has_model_entry()
		for _, entry in ipairs(S.list) do
			if entry.type == "model" then
				return true
			end
		end
		return false
	end
	local function initial_model_entry()
		if S.initial_model_recorded or has_model_entry() then
			return nil, true
		end
		local model = D.follow.llm() and D.follow.llm():current()
		if not model then
			return nil, false
		end
		return { type = "model", model = model.ref, effort = D.follow.llm():raw_effort(), at_entry_id = false }, true
	end
	function session.persist_message(_, _, ev)
		if type(ev) ~= "table" or type(ev.message) ~= "table" then
			lib.fail("invalid_arguments", "persist_message requires { message = table }")
		end
		local owner = D.store.check_owner(ev, true)
		D.store.check_new_id(ev.entry_id, true)
		local ext_id = ev.entry_id
		local readonly = S.readonly == true
		if not S.initial_model_recorded then
			local entry, recorded = initial_model_entry()
			if entry then
				local ok, err, qpath, qerr, err_code = add(
					entry,
					not readonly and S.file ~= nil,
					nil,
					nil,
					nil,
					ev.skip_initial_model_quarantine and { skip_quarantine = true } or nil
				)
				if not ok then
					D.store.fail_spi(err, qpath, qerr, false, qpath ~= nil, err_code)
				end
			end
			S.initial_model_recorded = recorded
		end
		local id
		local ok, err, qpath, qerr, err_code = D.store.run_host(function(valid)
			if valid and not valid() then
				return D.store.stale_result()
			end
			id = ext_id or new_id()
			local e = {
				type = "message",
				entry_id = id,
				parent_entry_id = S.leaf,
				message = ev.message,
				input_id = ev.message._input_id,
				transition_id = ev.message._transition_id,
				created_at_ms = lib.to_epoch_ms(mare.task.now()),
			}
			local persist_flag
			if readonly then
				persist_flag = false
			end
			local added, aerr, qp, qe, acode = add(e, persist_flag, not readonly and ev.durable, valid, function()
				S.ids[ev.message] = id
				S.leaf = id
				S.msg_count = S.msg_count + 1
				if not S.preview and ev.message.role == "user" and not ev.message.compaction then
					for _, block in ipairs(ev.message.content or {}) do
						if block.type == "text" then
							S.preview = block.text
							break
						end
					end
				end
			end, {
				skip_quarantine = ev.skip_quarantine and true or nil,
				bind_leaf = true,
			})
			if not added then
				return nil, aerr, qp, qe, acode
			end
			return true
		end, owner)
		if not ok then
			D.store.fail_spi(err, qpath, qerr, qpath ~= nil, false, err_code)
		end
		if readonly then
			return { status = "read_only", entry_id = id }
		end
		-- the index follows the session from its first message (not only at agent/end), so a
		-- crash mid-run still leaves it resumable from /resume
		if ev.message.role == "user" and not ev.message.notification then
			index_update()
		end
		return id
	end
	local function write_request_marker(kind, ev)
		if type(ev) ~= "table" or type(ev.request_id) ~= "string" or ev.request_id == "" then
			lib.fail("invalid_arguments", kind .. " requires nonempty request_id")
		end
		local owner = D.store.check_owner(ev, true)
		D.store.check_new_id(ev.entry_id, true)
		local id = ev.entry_id or new_id()
		if S.readonly then
			return { status = "read_only", entry_id = id }
		end
		local data = { request_id = ev.request_id, attempt = ev.attempt }
		local ok, err, qpath, qerr, err_code = D.store.run_host(function(valid)
			if valid and not valid() then
				return D.store.stale_result()
			end
			local added, aerr, qp, qe, acode = add({
				type = "custom",
				kind = kind,
				entry_id = id,
				data = data,
				created_at_ms = lib.to_epoch_ms(mare.task.now()),
			}, S.file ~= nil, ev.durable, valid, nil, ev.skip_quarantine and { skip_quarantine = true } or nil)
			if not added and S.file then
				return nil, aerr, qp, qe, acode
			end
			return true
		end, owner)
		if not ok then
			D.store.fail_spi(err, qpath, qerr, qpath ~= nil, false, err_code)
		end
		return id
	end
	function session.record_request(_, _, ev)
		return write_request_marker("agent/request", ev)
	end
	function session.record_request_end(_, _, ev)
		return write_request_marker("agent/request-end", ev)
	end
	function session.record_request_aborted(_, _, ev)
		return write_request_marker("agent/request-aborted", ev)
	end
	local function on_step_started()
		S.step_boundary = #S.list
	end
	local function on_turn_completed()
		S.step_boundary = nil
		index_update()
	end
	if D.lazy_recorder then
		D.recorder_events = {
			on_step_started = on_step_started,
			on_turn_completed = on_turn_completed,
		}
	else
		ctx:on("step/started", on_step_started)
		ctx:on("turn/completed", on_turn_completed)
	end
	-- A successful run is a fork boundary only after its final leaf is durable.
	function session.record_run_end(_, _, ev)
		if type(ev) ~= "table" then
			lib.fail("invalid_arguments", "record_run_end requires an options table")
		end
		local owner = D.store.check_owner(ev, true)
		if not S.leaf then
			return true
		end
		local ok, err, qpath, qerr, err_code = D.store.run_host(function(valid)
			if valid and not valid() then
				return D.store.stale_result()
			end
			local added, aerr, qp, qe, acode = add(
				{
					type = "custom",
					kind = "agent/run-end",
					data = { session_id = S.id, run_id = ev.run_id, leaf_entry_id = S.leaf },
					created_at_ms = lib.to_epoch_ms(mare.task.now()),
				},
				nil,
				true,
				valid,
				nil,
				{
					skip_quarantine = ev.skip_quarantine and true or nil,
					bind_leaf = function(es, st)
						local leaf = S.leaf
						es[1].data.leaf_entry_id = leaf
						st[1].data.leaf_entry_id = leaf
					end,
				}
			)
			if not added then
				return nil, aerr, qp, qe, acode
			end
			return true
		end, owner)
		if not ok then
			D.store.fail_spi(err, qpath, qerr, qpath ~= nil, false, err_code)
		end
		return true
	end
	-- The canonical input transaction commits accepted messages and the remaining inbox in
	-- one native batch, then publishes both together. Queue notifications are observations.
	function session.commit(_, _, batch, opts)
		opts = opts or {}
		batch = mare.json.decode(mare.json.encode(batch))
		if type(batch) ~= "table" or batch.version ~= 1 or type(batch.messages) ~= "table" then
			return nil, "invalid input commit"
		end
		if type(batch.inbox) == "table" then
			-- Re-project the entire inbox so secrets registered after enqueue never leak into `session/commit-ready`.
			batch.inbox = require("mare_lib").project_inbox(batch.inbox)
		end
		if S.readonly then
			return nil, "cannot durably queue input in a read-only session"
		end
		local owner, generation, epoch = S.id, S.generation, D.store.current_epoch()
		if (opts.session_id and opts.session_id ~= owner) or (opts.session_epoch and opts.session_epoch ~= epoch) then
			return nil, "input owner changed before commit", "stale_session", "pre_commit"
		end
		local res, err, c3, c4, c5, c6 = D.store.run_host(function(active)
			local operation
			if batch.operation then
				local op_err
				operation, op_err = D.operations.prepare(batch.operation)
				if not operation then
					return nil, op_err
				end
			end
			local vote = ctx.policy:decide(
				"session/commit-ready",
				table.pack({ inbox = batch.inbox, messages = batch.messages })
			)
			if not vote or vote.decision ~= "allow" then
				return nil, (vote and vote.reason) or "input commit refused"
			end
			local publication, current, messages = ctx.agent:prepare_commit(batch.inbox, batch.messages, opts.turn_id)
			local entries, ids, next_leaf = {}, {}, S.leaf
			local initial_model, recorded_initial_model
			if #messages > 0 then
				initial_model, recorded_initial_model = initial_model_entry()
				if initial_model then
					entries[#entries + 1] = initial_model
				end
			end
			local now_ms = lib.to_epoch_ms(mare.task.now())
			for i, message in ipairs(messages) do
				local id = new_id()
				ids[i] = id
				entries[#entries + 1] = {
					type = "message",
					entry_id = id,
					parent_entry_id = next_leaf,
					message = message,
					input_id = message._input_id,
					created_at_ms = now_ms,
				}
				next_leaf = id
			end
			entries[#entries + 1] = {
				type = "custom",
				kind = "agent/queue",
				data = batch.inbox,
				provenance = "session-host",
				created_at_ms = now_ms,
			}
			if operation then
				entries[#entries + 1] = operation.entry
			end
			local function valid()
				return (not operation or operation.valid())
					and (not active or active())
					and S.id == owner
					and S.generation == generation
					and current()
					and (not opts.cancel or not opts.cancel:is_cancelled())
			end
			local ok, err, _, _, err_code, commit_phase = D.store.add_batch(entries, valid, function()
				S.queue = batch.inbox
				if recorded_initial_model then
					S.initial_model_recorded = true
				end
				if #messages > 0 then
					S.leaf = next_leaf
				end
				for i, message in ipairs(messages) do
					S.ids[message] = ids[i]
					if message.role == "user" then
						S.turn_id = ids[i]
					end
				end
				S.msg_count = S.msg_count + #messages
				if operation then
					operation.install()
				end
				publication()
				if operation then
					operation.complete()
				end
			end, {
				persist = true,
				sync = true,
				bind_leaf = function(es, st)
					local leaf = S.leaf
					local cur = leaf
					local msg_idx = 0
					for i, entry in ipairs(es) do
						if entry.type == "message" then
							msg_idx = msg_idx + 1
							entry.parent_entry_id = cur
							st[i].parent_entry_id = cur
							cur = ids[msg_idx]
						end
					end
					next_leaf = cur
					return function()
						return #messages == 0 or S.leaf == leaf
					end
				end,
			})
			if not ok then
				return nil, "input commit failed: " .. tostring(err), err_code or "persist_failed", commit_phase
			end
			return {
				messages = messages,
				ids = ids,
				inbox = batch.inbox,
				operation = operation and operation.receipt or nil,
			}
		end, { id = owner, generation = generation })
		if not res then
			return nil, err, c3 or c5 or "persist_failed", c4 or c6 or "pre_commit"
		end
		return res
	end
end
