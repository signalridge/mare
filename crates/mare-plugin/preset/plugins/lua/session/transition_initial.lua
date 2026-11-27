-- Durable pending initial-handoff persistence, settlement, error propagation,
-- duplicate-execution protection, and crash recovery for session:transition.
return function(T)
	local ctx, D, S, session = T.ctx, T.D, T.S, T.session
	local owner_matches, copy_bounded = T.owner_matches, T.copy_bounded
	local in_flight = {}

	local function pending_persist()
		return not D.disabled and not (mare.args and mare.args.no_session)
	end

	local lib = require("mare_lib")
	local function list_all(kind)
		if not session.list_records then
			return {}
		end
		return lib.all_records({
			list_records = function(_, k, o)
				return session.list_records(session, ctx, k, o)
			end,
		}, kind) or {}
	end

	local function initial_records_for_session(session_id)
		local out = {}
		local recs = list_all("session/transition-initial")
		for _, rec in ipairs(recs) do
			if type(rec) == "table" and type(rec.target) == "table" and rec.target.session_id == session_id then
				out[#out + 1] = rec
			end
		end
		return out
	end

	local function latest_initial_record(id, session_id)
		local latest
		for _, rec in ipairs(initial_records_for_session(session_id)) do
			if rec.transition_id == id then
				latest = rec
			end
		end
		return latest
	end

	local function pending_for_current_session()
		local order, by_id = {}, {}
		for _, rec in ipairs(initial_records_for_session(S.id)) do
			if type(rec.transition_id) == "string" then
				if by_id[rec.transition_id] == nil then
					order[#order + 1] = rec.transition_id
				end
				by_id[rec.transition_id] = rec
			end
		end
		local out = {}
		for _, tid in ipairs(order) do
			local rec = by_id[tid]
			if rec and rec.status == "pending" and type(rec.initial) == "table" then
				out[#out + 1] = rec
			end
		end
		return out
	end

	local function record_status(id, mode, status, target, extra)
		if not owner_matches(target) or not session.add_record then
			return nil, "stale transition initial owner"
		end
		local persist = pending_persist()
		local data = {
			transition_id = id,
			mode = mode,
			status = status,
			target = { session_id = target.session_id, session_epoch = target.session_epoch },
		}
		if type(extra) == "table" then
			for k, v in pairs(extra) do
				data[k] = v
			end
		end
		local ok, res = pcall(session.add_record, session, ctx, "session/transition-initial", data, {
			persist = persist,
			sync = persist,
			session_id = target.session_id,
			session_epoch = target.session_epoch,
		})
		if not ok then
			return nil, type(res) == "table" and res.message or tostring(res)
		end
		return true
	end

	local function expected_turn_id(id, turn_id)
		if type(turn_id) == "string" and turn_id ~= "" then
			return turn_id
		end
		return "transition:" .. tostring(id)
	end

	local function has_completed_run(turn_id, session_id)
		local recs = list_all("agent/run-end")
		for _, rec in ipairs(recs) do
			if type(rec) == "table" and rec.session_id == session_id and rec.run_id == turn_id then
				return true
			end
		end
		return false
	end

	local function has_initial_user_message(id)
		for _, entry in ipairs(S.list or {}) do
			if entry.type == "message" and entry.transition_id == id then
				return true
			end
		end
		return false
	end

	local function has_tool_call(msg)
		if type(msg) ~= "table" or msg.role ~= "assistant" or type(msg.content) ~= "table" then
			return false
		end
		for _, b in ipairs(msg.content) do
			if type(b) == "table" and b.type == "tool_call" and not b.hosted then
				return true
			end
		end
		return false
	end

	local function last_branch_message()
		local ag = ctx.agent
		local msgs = ag and ag.messages and ag:messages()
		if type(msgs) == "table" and #msgs > 0 then
			return msgs[#msgs]
		end
		local last
		for _, entry in ipairs(S.list or {}) do
			if entry.type == "message" and type(entry.message) == "table" then
				last = entry.message
			end
		end
		return last
	end

	local function wait_turn(res)
		if type(res) == "table" and (res.status == "started" or res.status == nil) and res.turn_id then
			return ctx.agent:wait(res.turn_id)
		end
		return res
	end

	local function run_action(initial, id, target, turn_id)
		if has_completed_run(turn_id, target.session_id) then
			return { already_completed = true, turn_id = turn_id }
		end
		if initial.kind == "input" and has_initial_user_message(id) then
			if ctx.agent.interrupted and ctx.agent:interrupted() then
				return wait_turn(ctx.agent:continue({ turn_id = turn_id }))
			end
			local last = last_branch_message()
			if not last or last.role ~= "assistant" or last.interrupted or last.turn_aborted or has_tool_call(last) then
				return {
					error = "initial handoff interrupted before completion",
					termination_reason = "interrupted",
					turn_id = turn_id,
				}
			end
			return { already_completed = true, turn_id = turn_id }
		end
		if initial.kind == "input" then
			local opts = {
				source = initial.source,
				input_context = initial.input_context,
				turn_id = turn_id,
				transition_id = id,
			}
			if type(initial.content) == "string" then
				return wait_turn(ctx.agent:prompt(initial.content, opts))
			end
			local parts = {}
			if type(initial.content) == "table" then
				for _, block in ipairs(initial.content) do
					if type(block) == "table" and block.type == "text" and type(block.text) == "string" then
						parts[#parts + 1] = block.text
					end
				end
			end
			opts.content = initial.content
			return wait_turn(ctx.agent:prompt(table.concat(parts, "\n"), opts))
		elseif initial.kind == "continue" then
			local opts = {}
			if type(initial.opts) == "table" then
				for k, v in pairs(initial.opts) do
					opts[k] = v
				end
			end
			opts.turn_id = turn_id
			return wait_turn(ctx.agent:continue(opts))
		end
		return { error = "unsupported transition initial kind: " .. tostring(initial.kind) }
	end

	local function settle(initial, target, id, mode, cancel, turn_id)
		local req_turn_id = expected_turn_id(id, turn_id)
		if in_flight[id] then
			return nil, "duplicate transition initial execution", "rejected"
		end
		local existing = latest_initial_record(id, target.session_id)
		if existing and existing.status ~= "pending" then
			if existing.status == "settled" then
				return true, nil, nil, existing.turn_id
			end
			return nil, existing.error or ("transition initial already " .. tostring(existing.status)), "rejected"
		end
		in_flight[id] = true
		local inflight_guard <close> = setmetatable({}, {
			__close = function()
				in_flight[id] = nil
			end,
		})
		if not ctx.agent then
			local err = "agent service unavailable for initial handoff"
			if owner_matches(target) then
				record_status(id, mode, "failed", target, { error = err })
			end
			return nil, err, "rejected"
		end
		if not owner_matches(target) then
			return nil, "transition target owner changed", "stale"
		end
		if cancel and cancel:is_cancelled() then
			record_status(id, mode, "cancelled", target, { error = "transition cancelled" })
			return nil, "transition cancelled", "rejected"
		end
		local res = run_action(initial, id, target, req_turn_id)
		if not owner_matches(target) then
			return nil, "transition target owner changed", "stale"
		end
		local err
		if type(res) ~= "table" then
			err = "initial handoff returned no outcome"
		elseif res.error ~= nil then
			err = require("mare_lib").error_message(res.error)
		elseif res.termination_reason == "error" then
			err = "initial handoff failed"
		elseif res.termination_reason == "interrupted" then
			err = "initial handoff interrupted"
		elseif res.queued == true then
			err = "agent busy; initial handoff was queued instead of starting"
		end
		if err ~= nil or (cancel and cancel:is_cancelled()) then
			local cancelled = cancel and cancel:is_cancelled()
			local msg = err or "transition cancelled"
			local status = cancelled and "cancelled" or "failed"
			record_status(id, mode, status, target, {
				error = msg,
				not_sent = type(res) == "table" and res.not_sent == true or nil,
				turn_id = type(res) == "table" and res.turn_id or nil,
			})
			return nil, msg, "rejected"
		end
		local turn_id = type(res) == "table" and res.turn_id or nil
		local rec_ok, rec_err = record_status(id, mode, "settled", target, { turn_id = turn_id })
		if not rec_ok then
			return nil, rec_err or "failed to persist initial handoff settlement", "rejected"
		end
		return true, nil, nil, turn_id
	end

	local function build_pending_record(spec, id, source, target)
		return {
			kind = "session/transition-initial",
			scope = "session",
			data = {
				transition_id = id,
				turn_id = expected_turn_id(id),
				mode = spec.mode,
				status = "pending",
				baseline_messages = spec.messages and #spec.messages or 0,
				source = copy_bounded(source),
				target = { session_id = target.session_id, session_epoch = target.session_epoch },
				initial = copy_bounded(spec.initial),
			},
		}
	end

	local function complete_committed(spec, id, mode, source, target, cancel)
		ctx.bus:emit("session/transition-committed", {
			status = "committed",
			transition_id = id,
			mode = mode,
			source = copy_bounded(source),
			target = copy_bounded(target),
			has_initial = spec.initial ~= nil,
		})
		local outcome = { status = "applied", transition_id = id, mode = mode, source = source, target = target }
		if spec.initial then
			local ok, err, failure_status, turn_id =
				settle(spec.initial, target, id, mode, cancel, expected_turn_id(id))
			if not ok then
				local failed = {
					status = failure_status or "rejected",
					transition_id = id,
					mode = mode,
					source = source,
					target = owner_matches(target) and target or nil,
					error = tostring(err),
				}
				ctx.bus:emit("session/transition-failed", copy_bounded(failed))
				return failed, true
			end
			outcome.initial_status = "settled"
			outcome.turn_id = turn_id
		end
		ctx.bus:emit("session/transition-completed", copy_bounded(outcome))
		return outcome, false
	end

	local function validate_recovered_initial(rec)
		if type(rec.initial) ~= "table" then
			return nil, "recovered transition initial must be a table"
		end
		local initial = copy_bounded(rec.initial)
		local kind = initial.kind
		if kind == "input" then
			local content = initial.content
			if type(content) == "string" then
				if content == "" then
					return nil, "recovered initial input content cannot be empty"
				end
			elseif type(content) == "table" and #content > 0 then
				for k, b in pairs(content) do
					if type(k) ~= "number" or k % 1 ~= 0 or k < 1 or k > #content then
						return nil, "recovered initial input content must be a dense array"
					end
					if type(b) == "table" and b.type == "image" and b.attachment ~= nil then
						return nil, "recovered initial input cannot specify internal image attachment"
					end
				end
			else
				return nil, "recovered initial input content must be a non-empty string or content array"
			end
			if type(initial.source) ~= "string" or initial.source == "" then
				initial.source = "untrusted"
			end
			return initial
		elseif kind == "continue" then
			if initial.opts ~= nil and type(initial.opts) ~= "table" then
				return nil, "recovered initial continue opts must be a table"
			end
			return initial
		end
		return nil, "unsupported recovered transition initial kind"
	end

	local function recover_loaded()
		if D.disabled or S.readonly or S.unstable then
			return
		end
		local pending = pending_for_current_session()
		if #pending == 0 then
			return
		end
		local target = { session_id = S.id, session_epoch = D.store.current_epoch() }
		ctx:spawn(function()
			for _, rec in ipairs(pending) do
				if not owner_matches(target) or S.readonly or S.unstable or D.disabled then
					return
				end
				if ctx.agent and ctx.agent:activity().busy then
					return
				end
				local validated_initial, val_err = validate_recovered_initial(rec)
				local ok, err, failure_status, turn_id
				if validated_initial then
					ok, err, failure_status, turn_id =
						settle(validated_initial, target, rec.transition_id, rec.mode or "new", nil, rec.turn_id)
				else
					record_status(rec.transition_id, rec.mode or "new", "failed", target, { error = val_err })
					ok, err, failure_status = nil, val_err, "rejected"
				end
				if ok then
					ctx.bus:emit("session/transition-completed", {
						status = "applied",
						transition_id = rec.transition_id,
						mode = rec.mode or "new",
						source = copy_bounded(rec.source),
						target = target,
						initial_status = "settled",
						recovered = true,
						turn_id = turn_id,
					})
				elseif err ~= "duplicate transition initial execution" then
					ctx.bus:emit("session/transition-failed", {
						status = failure_status or "rejected",
						transition_id = rec.transition_id,
						mode = rec.mode or "new",
						source = copy_bounded(rec.source),
						target = owner_matches(target) and target or nil,
						error = tostring(err),
						recovered = true,
					})
				end
			end
		end)
	end

	return {
		pending_persist = pending_persist,
		build_pending_record = build_pending_record,
		complete_committed = complete_committed,
		recover_loaded = recover_loaded,
	}
end
