-- Portable clean-context ("new") transition orchestration over session.new,
-- D.store.add_batch, and durable initial-handoff settlement.
return function(D)
	local ctx, S, session = D.ctx, D.S, D.session
	local new_id = D.store.new_id
	local deferred = {}
	local deferred_running = {}
	local emitted_failures = {}

	local MAX_BYTES = 32 * 1024 * 1024
	local MAX_DEPTH = 48
	local MAX_NODES = 50000

	local function rejected(reason)
		return { status = "rejected", error = tostring(reason) }
	end

	local function owner_matches(source)
		return source.session_id == S.id and source.session_epoch == D.store.current_epoch()
	end

	local function is_token(v)
		return type(v) == "userdata" and type(v.is_cancelled) == "function" and type(v.child) == "function"
	end

	local function check_limits(value, depth, nodes)
		if depth > MAX_DEPTH then
			error("transition spec exceeds 48 levels", 0)
		end
		nodes.count = nodes.count + 1
		if nodes.count > MAX_NODES then
			error("transition spec exceeds 50000 nodes", 0)
		end
		if type(value) == "table" then
			for _, v in pairs(value) do
				check_limits(v, depth + 1, nodes)
			end
		end
	end

	local function copy_bounded(value)
		if value == nil then
			return nil
		end
		local encoded = mare.json.encode_strict(value)
		assert(#encoded <= MAX_BYTES, "transition spec exceeds 32 MiB")
		local decoded = mare.json.decode_lossless(encoded)
		check_limits(decoded, 0, { count = 0 })
		return decoded
	end

	local function is_dense_array(t)
		if type(t) ~= "table" then
			return false
		end
		local count = 0
		for k in pairs(t) do
			if type(k) ~= "number" or k < 1 or k % 1 ~= 0 then
				return false
			end
			count = count + 1
		end
		return count == #t
	end

	local function validate_spec(raw, mode_override)
		return pcall(function()
			assert(type(raw) == "table", "transition spec must be a table")
			local spec = copy_bounded(raw)
			local allowed_top = {
				mode = true,
				data = true,
				cwd = true,
				carry = true,
				records = true,
				messages = true,
				initial = true,
			}
			for k in pairs(spec or {}) do
				assert(allowed_top[k], "unknown transition spec field: " .. tostring(k))
			end
			local mode = mode_override or spec.mode or "new"
			assert(mode == "new" or mode == "workspace", "transition mode must be 'new' or 'workspace'")
			spec.mode = mode

			if mode == "workspace" then
				if spec.data ~= nil then
					assert(type(spec.data) == "table", "workspace transition data must be a table")
					for k in pairs(spec.data) do
						assert(k == "cwd" or k == "carry", "unknown workspace transition data field: " .. tostring(k))
					end
				end
				local cwd = (spec.data and spec.data.cwd) or spec.cwd
				assert(type(cwd) == "string" and #cwd > 0, "workspace transition requires a non-empty cwd")
				local carry = spec.carry
				if spec.data and spec.data.carry ~= nil then
					carry = spec.data.carry
				end
				assert(carry == nil or type(carry) == "boolean", "workspace transition carry must be a boolean")
			else
				assert(
					spec.data == nil and spec.cwd == nil and spec.carry == nil,
					"new-session transition does not accept workspace cwd/carry/data"
				)
			end

			if spec.records ~= nil then
				assert(is_dense_array(spec.records), "transition records must be a dense array")
				for _, rec in ipairs(spec.records) do
					assert(type(rec) == "table", "transition record must be a table")
					for k in pairs(rec) do
						assert(k == "kind" or k == "data" or k == "scope", "unknown transition record field")
					end
					assert(type(rec.kind) == "string" and #rec.kind > 0, "transition record requires kind")
					assert(
						not D.store.is_reserved_record_kind(rec.kind),
						"transition record cannot write reserved internal kind: " .. rec.kind
					)
					assert(
						rec.scope == nil or rec.scope == "session" or rec.scope == "branch",
						"transition record scope must be 'session' or 'branch'"
					)
				end
			end

			local function check_no_internal_attachments(content)
				if type(content) == "table" then
					for _, b in ipairs(content) do
						if type(b) == "table" and b.type == "image" and b.attachment ~= nil then
							error("transition content cannot specify internal image attachment", 0)
						end
					end
				end
			end

			if spec.messages ~= nil then
				assert(is_dense_array(spec.messages), "transition messages must be a dense array")
				for _, msg in ipairs(spec.messages) do
					assert(type(msg) == "table" and type(msg.role) == "string", "transition message requires role")
					check_no_internal_attachments(msg.content)
				end
			end

			if spec.initial ~= nil then
				assert(type(spec.initial) == "table", "transition initial action must be a table")
				local kind = spec.initial.kind
				if kind == "input" then
					for k in pairs(spec.initial) do
						assert(
							k == "kind" or k == "content" or k == "source" or k == "input_context",
							"unknown initial input field"
						)
					end
					local content = spec.initial.content
					if type(content) == "string" then
						assert(#content > 0, "initial input content cannot be empty")
					elseif is_dense_array(content) then
						assert(#content > 0, "initial input content array cannot be empty")
						check_no_internal_attachments(content)
					else
						error("initial input content must be a string or content block array", 0)
					end
					assert(
						spec.initial.source == nil
							or (type(spec.initial.source) == "string" and #spec.initial.source > 0),
						"initial input source must be a non-empty string"
					)
				elseif kind == "continue" then
					for k in pairs(spec.initial) do
						assert(k == "kind" or k == "opts", "unknown initial continue field")
					end
					assert(
						spec.initial.opts == nil or type(spec.initial.opts) == "table",
						"initial continue opts must be a table"
					)
				else
					error("transition initial.kind must be 'input' or 'continue'", 0)
				end
			end

			return spec
		end)
	end

	local function can_run_now()
		if mare.exec.current() or session.switching() then
			return false
		end
		if ctx.agent then
			local state = ctx.agent:activity()
			if state.busy or state.settling or (state.queued or 0) > 0 then
				return false
			end
		end
		return true
	end

	local initial_handoff = require("session.transition_initial")({
		ctx = ctx,
		D = D,
		S = S,
		session = session,
		owner_matches = owner_matches,
		copy_bounded = copy_bounded,
	})

	local function apply_target_spec(spec, id, source, target, cancel)
		if not (spec.records or spec.messages or spec.initial) then
			return true
		end
		local now = require("mare_lib").to_epoch_ms(mare.task.now())
		local entries, msg_ids, messages = {}, {}, {}
		local next_leaf = S.leaf
		for _, rec in ipairs(spec.records or {}) do
			entries[#entries + 1] = {
				type = "custom",
				kind = rec.kind,
				data = copy_bounded(rec.data),
				scope = rec.scope,
				created_at_ms = now,
			}
		end
		if spec.initial then
			local pending = initial_handoff.build_pending_record(spec, id, source, target)
			entries[#entries + 1] = {
				type = "custom",
				kind = pending.kind,
				data = pending.data,
				scope = pending.scope,
				provenance = "session-host",
				created_at_ms = now,
			}
		end
		for i, raw_msg in ipairs(spec.messages or {}) do
			local msg = copy_bounded(raw_msg)
			local mid = new_id()
			msg_ids[i] = mid
			messages[i] = msg
			entries[#entries + 1] = {
				type = "message",
				entry_id = mid,
				parent_entry_id = next_leaf,
				message = msg,
				created_at_ms = now,
			}
			next_leaf = mid
		end
		return D.store.run_host(function(active)
			local function valid()
				return (not active or active()) and owner_matches(target) and (not cancel or not cancel:is_cancelled())
			end
			local ok, err = D.store.add_batch(entries, valid, function()
				if #messages > 0 then
					S.leaf = next_leaf
					S.msg_count = S.msg_count + #messages
					for i, msg in ipairs(messages) do
						S.ids[msg] = msg_ids[i]
					end
					if ctx.agent then
						ctx.agent:set_messages(messages)
					end
					ctx.bus:emit("agent/messages-replaced", { messages = messages })
				end
			end)
			if not ok then
				return nil, err or "transition target commit failed"
			end
			return true
		end, { id = target.session_id, generation = S.generation })
	end

	local function run_new_transition(spec, id, source, cancel)
		if not owner_matches(source) then
			return { status = "stale" }
		end
		if cancel:is_cancelled() then
			return rejected("transition cancelled")
		end
		if mare.exec.current() or session.switching() or (ctx.agent and ctx.agent:activity().busy) then
			return rejected("session is busy")
		end
		local policy = ctx.policy:decide(
			"session/mutation-ready",
			table.pack({
				transition_id = id,
				kind = "session/transition",
				source = source,
				cancel = cancel,
			})
		)
		if not policy or policy.decision ~= "allow" then
			return rejected((policy and policy.reason) or "transition was refused")
		end
		if not owner_matches(source) or cancel:is_cancelled() then
			return rejected("transition source changed or cancelled")
		end
		local ok, err = pcall(D.lifecycle.new_session)
		if not ok then
			return rejected(err)
		end
		local target = { session_id = S.id, session_epoch = D.store.current_epoch() }
		local applied, apply_err = apply_target_spec(spec, id, source, target, cancel)
		if not applied then
			cancel:cancel()
			mare.task.fail_session("transition target initialization failed: " .. tostring(apply_err))
			return {
				status = "indeterminate",
				transition_id = id,
				mode = "new",
				source = source,
				error = tostring(apply_err),
			}
		end
		local outcome, failed_emitted = initial_handoff.complete_committed(spec, id, "new", source, target, cancel)
		if failed_emitted and deferred_running[id] then
			emitted_failures[id] = true
		end
		return outcome
	end

	function session.transition(_, _, raw_spec, opts)
		if type(opts) ~= "table" then
			return rejected("transition owner is required")
		end
		for key in pairs(opts) do
			if key ~= "session_id" and key ~= "session_epoch" and key ~= "cancel" and key ~= "defer" then
				return rejected("unknown transition option")
			end
		end
		if opts.defer ~= nil and type(opts.defer) ~= "boolean" then
			return rejected("defer must be a boolean")
		end
		if not opts.defer and mare.exec.current() then
			return rejected("transitions are unavailable inside tools without defer=true")
		end
		if not owner_matches(opts) then
			return { status = "stale" }
		end
		local parent = opts.cancel
		if parent ~= nil and not is_token(parent) then
			return rejected("cancel must be a token")
		end
		local cancel = parent and parent:child() or mare.task.token()
		if cancel:is_cancelled() then
			return rejected("transition cancelled")
		end
		local valid, spec = validate_spec(raw_spec)
		if not valid then
			return rejected(spec)
		end
		if spec.mode == "workspace" then
			return rejected("workspace switch is unavailable in this mode")
		end
		local source = { session_id = S.id, session_epoch = D.store.current_epoch() }
		local id = mare.crypto.random_hex(16)
		if opts.defer and not can_run_now() then
			local ast = ctx.agent and ctx.agent:activity() or nil
			if parent == nil and ast and ast.turn_cancelled then
				return rejected("transition cancelled")
			end
			deferred[#deferred + 1] = {
				id = id,
				spec = spec,
				source = source,
				cancel = cancel,
				turn_id = (parent == nil and ast and ast.busy) and ast.turn_id or nil,
			}
			return { status = "deferred", transition_id = id, mode = spec.mode, source = source }
		end
		local cancellation <close> = setmetatable({}, {
			__close = function()
				cancel:cancel()
			end,
		})
		return run_new_transition(spec, id, source, cancel)
	end

	local function run_deferred()
		if #deferred == 0 then
			return
		end
		local pending = deferred
		deferred = {}
		ctx:spawn(function()
			for _, item in ipairs(pending) do
				local outcome
				if not owner_matches(item.source) then
					outcome = {
						status = "stale",
						transition_id = item.id,
						mode = item.spec.mode,
						source = item.source,
					}
				elseif item.cancel:is_cancelled() then
					outcome = {
						status = "rejected",
						transition_id = item.id,
						mode = item.spec.mode,
						source = item.source,
						error = "transition cancelled",
					}
				else
					deferred_running[item.id] = true
					local running_guard <close> = setmetatable({}, {
						__close = function()
							deferred_running[item.id] = nil
						end,
					})
					outcome = session.transition(session, ctx, item.spec, {
						session_id = item.source.session_id,
						session_epoch = item.source.session_epoch,
						cancel = item.cancel,
					})
				end
				if outcome and outcome.status ~= "applied" and outcome.status ~= "queued" then
					if not emitted_failures[item.id] then
						ctx.bus:emit("session/transition-failed", table.copy(outcome))
					end
					emitted_failures[item.id] = nil
				end
			end
		end)
	end

	local function on_turn_completed(ev)
		if
			ev
			and #deferred > 0
			and (
				ev.status == "cancelled"
				or ev.termination_reason == "interrupted"
				or ev.cancellation_cause ~= nil
				or ev.error_kind == "aborted"
				or require("mare_lib").error_message(ev.error) == "aborted"
			)
		then
			for _, item in ipairs(deferred) do
				if (not ev.turn_id or item.turn_id == ev.turn_id) and not item.cancel:is_cancelled() then
					item.cancel:cancel()
				end
			end
		end
		-- what remains runs on agent/settled, once every end listener returned (no numeric
		-- priority on the bus to run last here)
	end

	local function on_agent_settled(ev)
		if (ev and ev.busy) or #deferred == 0 then
			return
		end
		run_deferred()
	end

	local function on_session_changed()
		if #deferred == 0 then
			return
		end
		local remaining = {}
		for _, item in ipairs(deferred) do
			if owner_matches(item.source) then
				remaining[#remaining + 1] = item
			else
				ctx.bus:emit("session/transition-failed", {
					status = "stale",
					transition_id = item.id,
					mode = item.spec.mode,
					source = item.source,
				})
			end
		end
		deferred = remaining
	end

	if D.lazy_transitions then
		D.transitions_events = {
			on_turn_completed = on_turn_completed,
			on_agent_settled = on_agent_settled,
			on_session_changed = on_session_changed,
		}
	else
		ctx:on("turn/completed", on_turn_completed)
		ctx:on("agent/settled", on_agent_settled)
		ctx:on("session/changed", on_session_changed)
	end

	-- after every session/loaded listener returned (D.emit_loaded, the emitter): a pending initial
	-- handoff may switch the session again, which the event's listeners must not see first
	D.after_loaded = initial_handoff.recover_loaded

	return {}
end
