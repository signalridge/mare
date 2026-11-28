-- agent.service: the `agent` service, notification turns, dispose, session and frontend events;
-- part of the `agent` plugin (plugins/agent.lua), loaded at the start of its apply.
local outcome = require("agent.outcome")
local wakeup = require("agent.wakeup")(outcome)
local poison_reason = mare.task.session_error

local function typed_err(code, message, phase)
	return setmetatable({ code = code, message = tostring(message), phase = phase }, {
		__tostring = function(t)
			return t.message
		end,
	})
end

local function enqueue(s, queue, text, opts, operation, on_commit, expected_owner, sess_override)
	local kind = queue == s.A.steer and "steer" or "follow_up"
	local entry = s.entry(text, opts)
	local ok, err = pcall(
		s.D.inputs.mutate,
		function(q)
			q[kind][#q[kind] + 1] = entry
			local op
			if type(operation) == "function" then
				op = operation(entry.input_id)
			elseif operation ~= nil then
				op = operation
			else
				op = opts and opts.operation or nil
			end
			return true, true, op
		end,
		kind == "steer" and s.A.turn_id or nil,
		function()
			if on_commit then
				on_commit(entry.input_id)
			end
			if opts and opts.turn_id then
				pcall(s.ctx.bus.emit, s.ctx.bus, "agent/commit", { turn_id = opts.turn_id, kind = "queued" })
			end
		end,
		opts and opts.cancel,
		expected_owner,
		sess_override
	)
	if not ok then
		return nil, err, entry.input_id
	end
	return entry.input_id
end

local function copy_result(val)
	if type(val) ~= "table" then
		return val
	end
	local out = {}
	for k, v in pairs(val) do
		out[k] = copy_result(v)
	end
	if mare.json.is_array(val) then
		return mare.json.array(out)
	end
	return out
end

local function session_receipt_key(s)
	local session = s.D.follow.session()
	local info = session and session:info()
	if info then
		return tostring(info.session_id or "")
	end
	return tostring(s.A.session_id or "")
end

local function op_cancel_token(opts)
	if opts and type(opts.cancel) == "userdata" then
		return opts.cancel
	end
	return mare.task.token()
end

local function valid_receipt_result(receipt, rid, kind, fp)
	if type(receipt) ~= "table" then
		return false
	end
	if receipt.request_id ~= rid or receipt.kind ~= kind or receipt.fingerprint ~= fp then
		return false
	end
	local res = receipt.result
	if type(res) ~= "table" then
		return false
	end
	if kind == "agent.enqueue" and (type(res.input_id) ~= "string" or res.input_id == "") then
		return false
	end
	return true
end

local function replay_result_table(receipt_res)
	local dup = copy_result(receipt_res or {})
	if type(dup) == "table" then
		dup.duplicate = true
		if type(dup.removed) == "table" then
			mare.json.array(dup.removed)
		end
	end
	return dup
end

local function begin_session_op(s, rid, kind, fp, opts, on_error, expected_owner, sess_override)
	if not rid then
		return nil, false
	end
	local session = sess_override or s.D.follow.session()
	if not (session and type(session.claim_operation) == "function") then
		if opts and (opts.require_claim or opts.return_typed_error) then
			local msg = "session provider does not support atomic idempotency receipts"
			if opts.return_typed_error then
				return nil, false, typed_err("rejected", msg, "pre_commit")
			end
			if on_error then
				on_error(msg)
			end
			error(msg, 0)
		end
		return nil, false
	end
	local identity = {
		request_id = rid,
		kind = kind,
		fingerprint = fp,
		session_id = expected_owner and expected_owner.session_id or nil,
		session_epoch = expected_owner and expected_owner.session_epoch or nil,
	}
	local claim, claim_err, claim_code = session:claim_operation(identity, op_cancel_token(opts))
	if not claim then
		local lib = require("mare_lib")
		local err_str = tostring(claim_err)
		if claim_code == "conflict" or (claim_code == nil and err_str:find("conflict", 1, true)) then
			local _, err_obj = pcall(
				lib.fail,
				"request_id_conflict",
				("request_id %q was already used for a different payload"):format(rid)
			)
			if on_error then
				on_error(err_obj)
			end
			error(err_obj, 0)
		end
		if opts and opts.return_typed_error then
			return nil, false, typed_err(claim_code or "rejected", claim_err, "pre_commit")
		end
		if on_error then
			on_error(claim_err)
		end
		error(claim_err, 0)
	end
	if claim.status == "replayed" then
		if not valid_receipt_result(claim.receipt, rid, kind, fp) then
			local msg = "session provider returned an invalid operation receipt"
			if opts and opts.return_typed_error then
				return nil, false, typed_err("invalid_operation_receipt", msg, "pre_commit")
			end
			if on_error then
				on_error(msg)
			end
			error(msg, 0)
		end
		return copy_result(claim.receipt.result), true
	end
	local state = { committed = false }
	local guard = setmetatable({}, {
		__close = function()
			if not state.committed and session.release_operation then
				pcall(session.release_operation, session, claim, "aborted before commit")
			end
		end,
	})
	return {
		claim = claim,
		identity = identity,
		session = session,
		state = state,
		guard = guard,
	}, false
end

local function build_session_op(session_op, result)
	if not session_op then
		return nil
	end
	return {
		request_id = session_op.identity.request_id,
		kind = session_op.identity.kind,
		fingerprint = session_op.identity.fingerprint,
		claim_id = session_op.claim.claim_id,
		result = result,
	}
end

local function validate_request_id(opts)
	if not opts or opts.request_id == nil then
		return nil
	end
	local rid = opts.request_id
	if type(rid) ~= "string" or #rid == 0 or #rid > 512 then
		require("mare_lib").fail("invalid_arguments", "request_id must be a non-empty string up to 512 bytes")
	end
	return rid
end

local function settled_receipt(store, skey, rid, fp)
	local by_req = store[skey]
	local rec = by_req and by_req[rid]
	if not rec then
		return nil
	end
	if rec.fingerprint ~= fp then
		require("mare_lib").fail(
			"request_id_conflict",
			("request_id %q was already used for a different payload"):format(rid)
		)
	end
	local dup = copy_result(rec.result)
	if type(dup) == "table" then
		dup.duplicate = true
	end
	return dup
end

local function record_receipt(store, skey, rid, fp, result)
	if not rid then
		return
	end
	local by_req = store[skey]
	if not by_req then
		by_req = {}
		store[skey] = by_req
	end
	by_req[rid] = { fingerprint = fp, result = copy_result(result) }
end

local function replay_of(settled)
	if settled.err ~= nil then
		error(settled.err, 0)
	end
	local dup = copy_result(settled.result)
	if type(dup) == "table" then
		dup.duplicate = true
	end
	return dup
end

-- In-memory `request_id` idempotency shared by `agent.prompt` and `agent.steer`, checked before
-- anything else runs (frontend routing included). A settled receipt replays with
-- `duplicate = true`; a resend while the first request is still in flight waits for its outcome;
-- a different payload under the same id is `request_id_conflict`. Returns `nil, replay` for a
-- resend, else a claim: `claim.ok(result)` records the receipt, `claim.err(err)` hands the error
-- to waiting resends (first outcome wins), and closing `claim.guard` unsettled fails them.
local function claim_request(receipts, pending, skey, rid, fp, what)
	local replayed = settled_receipt(receipts, skey, rid, fp)
	if replayed then
		return nil, replayed
	end
	local by_pending = pending[skey]
	local inflight = by_pending and by_pending[rid]
	if inflight then
		if inflight.fingerprint ~= fp then
			require("mare_lib").fail(
				"request_id_conflict",
				("request_id %q was already used for a different %s"):format(rid, what)
			)
		end
		if inflight.settled ~= nil then
			return nil, replay_of(inflight.settled)
		end
		local tx, rx = mare.task.channel()
		inflight.waiters[#inflight.waiters + 1] = tx
		return nil, replay_of(rx:recv())
	end
	if not by_pending then
		by_pending = {}
		pending[skey] = by_pending
	end
	local entry = { fingerprint = fp, waiters = {}, settled = nil }
	by_pending[rid] = entry
	local function publish(msg)
		if entry.settled ~= nil then
			return
		end
		entry.settled = msg
		for _, w in ipairs(entry.waiters) do
			w:send(msg)
		end
	end
	local claim = {}
	function claim.ok(result)
		if entry.settled == nil then
			record_receipt(receipts, skey, rid, fp, result)
		end
		publish({ result = copy_result(result) })
	end
	function claim.err(err_val)
		publish({ err = err_val })
	end
	claim.guard = setmetatable({}, {
		__close = function()
			if pending[skey] and pending[skey][rid] == entry then
				pending[skey][rid] = nil
			end
			publish({ err = what .. " cancelled before completion" })
		end,
	})
	return claim
end

local function register_turn_live(s, turn_id)
	s.turns_live[turn_id] = {
		turn_id = turn_id,
		session_gen = s.turn_session_gen,
		session_id = s.A.session_id,
		started_at_ms = require("mare_lib").to_epoch_ms(mare.task.now()),
		waiters = {},
	}
end

local function reset_turn_cache(s)
	s.turn_session_gen = (s.turn_session_gen or 0) + 1
	s.turns = {}
	s.turn_order = {}
	s.last_turn_id = nil
end

local function remove_settling_turn(s, entry)
	for i = #s.settling_turns, 1, -1 do
		if s.settling_turns[i] == entry then
			table.remove(s.settling_turns, i)
			return
		end
	end
end

local function current_settling_turn(s)
	for i = #s.settling_turns, 1, -1 do
		local entry = s.settling_turns[i]
		if entry.session_gen == s.turn_session_gen and entry.session_id == s.A.session_id then
			return entry.turn_id
		end
	end
	return nil
end

local function switching(s)
	local session = s.D.follow.session()
	return session and require("mare_lib").callable(session.switching) and session:switching()
end

local function compacting(s)
	local compact = s.D.follow and s.D.follow.compact and s.D.follow.compact()
	return compact and require("mare_lib").callable(compact.busy) and compact:busy()
end

local function writable_session_state(s, require_durable, check_inflight)
	local sess = s.D.follow.session and s.D.follow.session()
	local info = sess and sess:info()
	if require_durable then
		if
			mare.mode == "child"
			or not info
			or info.ephemeral
			or info.readonly
			or info.unstable
			or info.resume_error
		then
			return nil, nil, "session is not durable and writable", "rejected"
		end
	elseif info and (info.readonly or info.unstable or info.resume_error) then
		return nil, nil, "session is not writable and ready", "rejected"
	end
	if check_inflight then
		if s.A.persist_error or s.A.persist_inflight > 0 or #s.A.pending > 0 then
			return nil, nil, "session is not writable and ready", "rejected"
		end
		if s.A.token and s.A.token:is_cancelled() then
			return nil, nil, "automatic admission is blocked while cancellation settles", "rejected"
		end
	end
	return sess, info
end

-- Keep the run busy until its completed boundary is persisted, so a switch
-- cannot publish a different owner while the marker is being written.
local function complete_run(s, ok, res)
	if ok and not res.error then
		local written, err = s.persist({ kind = "run-end", payload = { run_id = s.A.turn_id }, durable = true })
		if not written then
			res.error = require("mare_lib").format_save_error(
				err,
				s.A.quarantine_paths or s.A.quarantine_path,
				s.A.quarantine_error
			)
			res.error_kind = "save_failed"
			res.termination_reason = "error"
		end
	end
	return ok, res
end

local function project_messages(messages, opts)
	if not messages then
		return nil
	end
	if not mare.projection.active() then
		return messages
	end
	local out = mare.json.array()
	for i, msg in ipairs(messages) do
		if type(msg) == "table" and msg.turn_id ~= nil then
			local tid = msg.turn_id
			local cleaned = table.copy(msg)
			cleaned.turn_id = nil
			local proj = mare.projection.project(cleaned, opts)
			if type(proj) == "table" then
				proj.turn_id = type(tid) == "string" and (mare.projection.project(tid, opts)) or tid
			end
			out[i] = proj
		else
			out[i] = mare.projection.project(msg, opts)
		end
	end
	return out
end

local function publish_end(s, res, turn_id)
	local lib = require("mare_lib")
	local ended_token = s.A.token
	res.turn_id = turn_id
	local live = s.turns_live[turn_id]
	local turn_session_gen = (live and live.session_gen) or s.turn_session_gen
	local turn_session_id = (live and live.session_id) or s.A.session_id
	local settling_entry = {
		turn_id = turn_id,
		session_gen = turn_session_gen,
		session_id = turn_session_id,
	}
	s.settling_turns[#s.settling_turns + 1] = settling_entry
	local settled_record = false
	-- Keep `s.turns_live[turn_id]` and `settling_entry` registered while `turn/completed`
	-- and `agent/settled` run so both explicit `agent.wait(turn_id)` and implicit
	-- `agent.wait()` resolve to this turn until its record is committed.
	local _live_guard <close> = setmetatable({}, {
		__close = function()
			remove_settling_turn(s, settling_entry)
			if not settled_record and s.turns_live[turn_id] == live then
				s.turns_live[turn_id] = nil
			end
		end,
	})
	local err_text = lib.error_message(res.error)
	local status = (res.termination_reason == "interrupted") and "cancelled"
		or ((err_text and err_text ~= "") and "failed" or "completed")
	local ev_err = lib.structured_error(
		res.error,
		res.error_code or res.error_kind or (status == "cancelled" and "cancelled") or "error",
		res.error_data
	)
	-- `turn/completed` dispatches still running (a listener may run a whole nested prompt).
	s.settling = s.settling + 1
	do
		local _guard <close> = setmetatable({}, {
			__close = function()
				s.settling = s.settling - 1
			end,
		})
		s.ctx.bus:emit("turn/completed", {
			turn_id = turn_id,
			status = status,
			termination_reason = res.termination_reason,
			cancellation_cause = res.cancellation_cause,
			text = res.text,
			structured = res.structured,
			usage = res.usage,
			error = ev_err,
			error_kind = res.error_kind,
			not_sent = res.not_sent,
			output_truncated = res.truncated,
			stop_reason = res.stop_reason,
		})
	end
	if not s.A.busy and s.A.turn_id == turn_id then
		s.A.turn_id = nil
	end
	-- Cancelled end listeners cannot immediately restart an automatic chain. Once
	-- that boundary settles, a later explicit resume may use idle admission again.
	if s.A.token == ended_token and ended_token and ended_token:is_cancelled() and not s.A.busy then
		s.A.token = nil
	end
	-- After every end listener returned: busy when another run, or an outer end dispatch, is
	-- still in progress (listeners wanting "idle" wait for busy == false).
	s.ctx.bus:emit("agent/settled", { turn_id = turn_id, busy = s.A.busy or s.settling > 0 })
	local record = {
		turn_id = turn_id,
		status = status,
		error = ev_err,
		error_kind = res.error_kind,
		termination_reason = res.termination_reason,
		cancellation_cause = res.cancellation_cause,
		save_warning = res.save_warning and mare.projection.project(res.save_warning, { lossy = true }) or nil,
		usage = res.usage or { input = 0, output = 0, total = 0 },
		text = res.text and mare.projection.project(res.text, { lossy = true }) or nil,
		messages = project_messages(res.messages, { lossy = true }),
		structured = res.structured and mare.projection.project(res.structured, { lossy = true }) or nil,
		not_sent = res.not_sent,
		truncated = res.truncated,
		stop_reason = res.stop_reason,
		started_at_ms = (live and live.started_at_ms) or require("mare_lib").to_epoch_ms(mare.task.now()),
		completed_at_ms = require("mare_lib").to_epoch_ms(mare.task.now()),
	}
	s.turns_live[turn_id] = nil
	remove_settling_turn(s, settling_entry)
	settled_record = true
	if turn_session_gen == s.turn_session_gen and turn_session_id == s.A.session_id then
		s.turns[turn_id] = record
		s.last_turn_id = turn_id
		s.turn_order[#s.turn_order + 1] = turn_id
		if #s.turn_order > 64 then
			local oldest = table.remove(s.turn_order, 1)
			s.turns[oldest] = nil
		end
	end
	if live then
		for _, ch in ipairs(live.waiters) do
			ch:send(record)
		end
	end
end

local function claim_prompt(s, opts)
	s.A.busy = true
	s.A.run_id = s.A.run_id + 1
	s.A.token = mare.task.token()
	s.A.cancel_reason = nil
	-- turn id; every event of the run is tagged with it
	s.A.turn_id = (opts and opts.turn_id) or mare.crypto.random_hex(8)
	register_turn_live(s, s.A.turn_id)
	return s.A.turn_id
end

local function finalize_turn(s, run_session_id, ok, res, turn_id)
	ok, res = complete_run(s, ok, res)
	s.A.busy = false
	if not ok then
		res = outcome.failure(res)
	end
	wakeup.park_failed_inputs(s, res)
	publish_end(s, res, turn_id)
	if not s.disposed and s.A.session_id == run_session_id and not (res and res.not_sent) then
		wakeup.wake_pending(s, run_session_id)
	end
	return res
end

local function finish_prompt(s, text, opts, turn_id)
	-- a new prompt abandons an interrupted request: it won't be retried, its crash mark ends
	if s.A.resume_request then
		local okc, reset_path = pcall(mare.crash.finish, "req:" .. s.A.resume_request.request_id)
		if okc and reset_path then
			local note = ("crash guard state was corrupt and reset (backup: %s); crash counts were cleared"):format(
				tostring(reset_path)
			)
			mare.log("warn", note)
			if mare.ui and mare.ui.notify then
				mare.ui.notify(note, "warning")
			end
		end
		s.A.resume_request = nil
	end
	local voters_before_transform = s.D.inputs.has_input_ready_voters()
	local run_opts = opts
	if voters_before_transform then
		run_opts = {}
		for k, v in pairs(opts or {}) do
			run_opts[k] = v
		end
		run_opts.initial_voters_seen = true
	end
	local run_session_id = s.A.session_id
	local ok, res = pcall(function()
		-- `source`: who sent a prompt that isn't the user's (e.g. "peer": another session's
		-- message) — listeners that count user turns (jobs' wake-up limit) skip those
		s.ctx.bus:emit("turn/started", {
			turn_id = turn_id,
			trigger = opts.source or (opts.queued and "queued") or "prompt",
			text = text and mare.projection.project(text),
			images = opts.images and mare.projection.project(opts.images),
			content = opts.content and mare.projection.project(opts.content),
			image_markers = opts.image_markers and mare.projection.project(opts.image_markers),
			source = opts.source,
		})
		if run_opts.queued then
			return s.run(nil, run_opts)
		end
		local user = s.build_user(text, run_opts.images, run_opts.content, run_opts.source, run_opts.peer_message)
		if run_opts.transition_id then
			user._transition_id = run_opts.transition_id
		end
		return s.run(user, run_opts)
	end)
	return finalize_turn(s, run_session_id, ok, res, turn_id)
end

local function has_tool_call(msg)
	if type(msg) ~= "table" or msg.role ~= "assistant" or type(msg.content) ~= "table" then
		return false
	end
	for _, b in ipairs(msg.content) do
		if type(b) == "table" and b.type == "tool_call" then
			return true
		end
	end
	return false
end

-- `expected_session` = the session a caller keyed its idempotency record to (RPC `requestId`).
-- Checked with no yield before the queue or turn is claimed, so a prompt can't be admitted into
-- a session other than the one its receipt belongs to.
local function session_moved(s, expected)
	if expected == nil then
		return false
	end
	local session = s.D.follow.session()
	local info = session and session:info()
	return type(expected) ~= "table"
		or not info
		or info.session_id ~= expected.session_id
		or info.session_epoch ~= expected.session_epoch
end
local SESSION_MOVED = "the session changed before this prompt was admitted; retry"

local function strip_expected_session(opts)
	if not opts or opts.expected_session == nil then
		return nil, opts
	end
	local own = {}
	for k, v in pairs(opts) do
		own[k] = v
	end
	own.expected_session = nil
	return opts.expected_session, own
end

local function validate_tools_opt(tools)
	if tools == nil then
		return nil
	end
	if type(tools) ~= "table" then
		return "tools must be an array of tool names or definitions"
	end
	for _, t in pairs(tools) do
		if type(t) ~= "string" and (type(t) ~= "table" or type(t.name) ~= "string") then
			return "each tool entry must be a string or table with a string `name`"
		end
	end
	return nil
end

local function route_frontend(s, op, text, opts)
	local routed = s.ctx.bus:bail("agent/frontend-route", { op = op, text = text, opts = opts })
	if type(routed) == "table" and routed.handled then
		if routed.error ~= nil then
			if type(routed.error) == "table" and type(routed.error.code) == "string" then
				require("mare_lib").fail(
					routed.error.code,
					routed.error.message or "rejected by active frontend",
					routed.error.data
				)
			end
			error(routed.error, 0)
		end
		return true, routed.result
	end
	return false
end

local function agent_prompt(s, _, _, raw_input, opts)
	local lib = require("mare_lib")
	local expected
	expected, opts = strip_expected_session(opts or {})
	local when_busy = opts.when_busy or "queue"
	if when_busy ~= "queue" and when_busy ~= "reject" then
		lib.fail("invalid_arguments", "when_busy must be `queue` or `reject`")
	end
	local rid = validate_request_id(opts)
	local text, images, image_markers, content = s.normalize_input(raw_input, opts)
	if
		not (opts and opts.queued == true)
		and text:match("^%s*$")
		and not (images and #images > 0)
		and not (content and #content > 0)
		and not opts.custom_message
	then
		lib.fail("invalid_arguments", "prompt input must not be empty")
	end
	local tools_err = validate_tools_opt(opts.tools)
	if tools_err then
		lib.fail("invalid_arguments", tools_err)
	end
	local run_opts = {}
	for k, v in pairs(opts) do
		run_opts[k] = v
	end
	run_opts.internal_notification = nil
	run_opts.notification = nil
	run_opts.images = images and lib.copy_deep(images) or nil
	run_opts.image_markers = image_markers and lib.copy_deep(image_markers) or nil
	run_opts.content = content and lib.copy_deep(content) or nil
	run_opts.tools = run_opts.tools and lib.copy_deep(run_opts.tools) or nil
	run_opts.output_schema = run_opts.output_schema and lib.copy_deep(run_opts.output_schema) or nil
	local schema_err = s.validate_output_schema(run_opts)
	if schema_err then
		lib.fail("invalid_arguments", schema_err)
	end
	local media_err, media_cap = s.check_media_support(run_opts)
	if media_err then
		lib.fail("unsupported", media_err, { capability = media_cap })
	end
	local failure = poison_reason()
	if failure then
		error(failure, 0)
	end

	local skey, fp, claim
	if rid then
		skey = session_receipt_key(s)
		fp = mare.crypto.sha256(mare.json.encode({
			input = raw_input,
			when_busy = when_busy,
			queue_only = opts.queue_only,
			output_schema = opts.output_schema,
			system = opts.system,
			tools = opts.tools,
			effort = opts.effort,
		}))
		local replayed
		claim, replayed = claim_request(s.prompt_receipts, s.prompt_pending, skey, rid, fp, "prompt")
		if replayed then
			return replayed
		end
	end
	local function settle_ok(res)
		if claim then
			claim.ok(res)
		end
	end
	local function settle_err(err_val)
		if claim then
			claim.err(err_val)
		end
	end
	local function fail_settled(code, msg, data)
		local _, err_obj = pcall(lib.fail, code, msg, data)
		settle_err(err_obj)
		error(err_obj, 0)
	end
	local _close_pending <close> = claim and claim.guard

	if session_moved(s, expected) then
		fail_settled("stale_session", SESSION_MOVED)
	end
	if switching(s) then
		fail_settled("busy", "session switch in progress; retry")
	end
	do
		local sess = s.D.follow.session and s.D.follow.session()
		local sess_info = sess and sess:info()
		if sess_info and type(sess_info.resume_error) == "string" and sess_info.resume_error ~= "" then
			fail_settled(
				"unavailable",
				("startup blocked: %s — session.create or session.load can fix it"):format(sess_info.resume_error)
			)
		end
	end

	local ok_route, handled, routed = pcall(route_frontend, s, "prompt", text, run_opts)
	if not ok_route then
		settle_err(handled)
		error(handled, 0)
	end
	if handled then
		settle_ok(routed)
		return routed
	end

	local session_op, session_replayed = begin_session_op(s, rid, "agent.prompt", fp, opts, settle_err)
	local _release_op <close> = session_op and session_op.guard
	if session_replayed then
		local orig = copy_result(session_op)
		settle_ok(orig)
		return replay_result_table(orig)
	end
	if s.A.busy or run_opts.queue_only == true then
		if s.A.busy and when_busy == "reject" then
			fail_settled("busy", "agent is busy")
		end
		if not s.A.busy and compacting(s) then
			fail_settled("busy", "compaction in progress; retry")
		end
		if opts.output_schema ~= nil then
			fail_settled(
				"invalid_arguments",
				"output_schema is not supported when queueing a follow-up on a busy agent"
			)
		end
		local input_id, err = enqueue(s, s.A.follow, text, run_opts, function(eid)
			return build_session_op(session_op, { status = "queued", input_id = eid })
		end, function(eid)
			if session_op then
				session_op.state.committed = true
			end
			if run_opts.queue_only then
				wakeup.park_input(s, eid)
			end
			settle_ok({ status = "queued", input_id = eid })
		end)
		if not input_id then
			if tostring(err):find(SESSION_MOVED, 1, true) then
				fail_settled("stale_session", tostring(err))
			end
			settle_err(err)
			error(err, 0)
		end
		if not s.A.busy then
			wakeup.wake_pending(s)
		end
		return { status = "queued", input_id = input_id }
	end
	if session_op then
		if session_op.session and session_op.session.release_operation then
			pcall(
				session_op.session.release_operation,
				session_op.session,
				session_op.claim,
				"released for asynchronous turn"
			)
		end
		session_op.state.committed = true
		session_op = nil
	end
	if compacting(s) then
		fail_settled("busy", "compaction in progress; retry")
	end
	if when_busy == "reject" then
		local _, _, ready_err = writable_session_state(s, false, true)
		if ready_err then
			fail_settled("agent.rejected", ready_err)
		end
	end
	if run_opts.messages then
		s.A.messages = run_opts.messages
		local target = s.ctx.llm:current()
		if run_opts.history_model and target and run_opts.history_model ~= target.ref then
			local ok, err = pcall(function()
				s.ctx.llm:restore_selection({
					model = run_opts.history_model,
					effort = run_opts.history_effort,
				})
				s.ctx.llm:set_model(target.ref, { target = "session" })
			end)
			if not ok then
				local failure_res = outcome.failure(err, s.A.messages)
				fail_settled("agent.rejected", failure_res.error)
			end
		end
	end
	local turn_id = claim_prompt(s, run_opts)
	local started = { status = "started", turn_id = turn_id }
	settle_ok(started)
	if s.ctx and s.ctx.kernel and s.ctx.kernel.mark_call_committed then
		s.ctx.kernel:mark_call_committed()
	end
	s.ctx.bus:emit("agent/commit", { turn_id = turn_id, kind = "started" })
	s.ctx:spawn(function()
		finish_prompt(s, text, run_opts, turn_id)
	end)
	return started
end

local function agent_continue(s, self, caller, opts)
	local lib = require("mare_lib")
	local expected
	expected, opts = strip_expected_session(opts or {})
	local failure = poison_reason()
	if failure then
		error(failure, 0)
	end
	local handled, routed = route_frontend(s, "continue", nil, opts)
	if handled then
		return routed
	end
	if session_moved(s, expected) then
		lib.fail("stale_session", SESSION_MOVED)
	end
	if switching(s) then
		lib.fail("busy", "session switch in progress; retry")
	end
	do
		local sess = s.D.follow.session and s.D.follow.session()
		local sess_info = sess and sess:info()
		if sess_info and type(sess_info.resume_error) == "string" and sess_info.resume_error ~= "" then
			lib.fail(
				"unavailable",
				("startup blocked: %s — session.create or session.load can fix it"):format(sess_info.resume_error)
			)
		end
	end
	if s.A.busy then
		lib.fail("busy", "agent is busy")
	end
	if compacting(s) then
		lib.fail("busy", "compaction in progress; retry")
	end
	local run_opts = {}
	for k, v in pairs(opts) do
		run_opts[k] = v
	end
	if not self.interrupted(self, caller) then
		if #s.A.steer > 0 or #s.A.follow > 0 then
			run_opts.queued = true
		else
			lib.fail("agent.no_pending_work", "nothing to continue")
		end
	end
	local tools_err = validate_tools_opt(opts.tools)
	if tools_err then
		lib.fail("invalid_arguments", tools_err)
	end
	local schema_err = s.validate_output_schema(run_opts)
	if schema_err then
		lib.fail("invalid_arguments", schema_err)
	end
	local run_session_id = s.A.session_id
	local turn_id = claim_prompt(s, run_opts)
	if s.ctx and s.ctx.kernel and s.ctx.kernel.mark_call_committed then
		s.ctx.kernel:mark_call_committed()
	end
	s.ctx:spawn(function()
		s.ctx.bus:emit("turn/started", { turn_id = turn_id, trigger = run_opts.queued and "queued" or "continue" })
		local ok, res = pcall(s.run, nil, run_opts)
		finalize_turn(s, run_session_id, ok, res, turn_id)
	end)
	return { turn_id = turn_id }
end

local function agent_steer(s, _, _, raw_input, opts)
	local lib = require("mare_lib")
	opts = opts or {}
	local rid = validate_request_id(opts)
	local expected_turn = opts.expected_turn_id
	local text, images, image_markers, content = s.normalize_input(raw_input, opts)
	if
		text:match("^%s*$")
		and not (images and #images > 0)
		and not (content and #content > 0)
		and not opts.custom_message
	then
		lib.fail("invalid_arguments", "steer input must not be empty")
	end
	local run_opts = {}
	for k, v in pairs(opts) do
		run_opts[k] = v
	end
	run_opts.internal_notification = nil
	run_opts.notification = nil
	run_opts.images = images and lib.copy_deep(images) or nil
	run_opts.image_markers = image_markers and lib.copy_deep(image_markers) or nil
	run_opts.content = content and lib.copy_deep(content) or nil
	run_opts.tools = run_opts.tools and lib.copy_deep(run_opts.tools) or nil
	run_opts.output_schema = run_opts.output_schema and lib.copy_deep(run_opts.output_schema) or nil
	local media_err, media_cap = s.check_media_support(run_opts)
	if media_err then
		lib.fail("unsupported", media_err, { capability = media_cap })
	end
	local skey, fp, claim
	if rid then
		skey = session_receipt_key(s)
		fp = mare.crypto.sha256(mare.json.encode({
			input = raw_input,
			expected_turn_id = expected_turn or false,
		}))
		local replayed
		claim, replayed = claim_request(s.steer_receipts, s.steer_pending, skey, rid, fp, "steer")
		if replayed then
			return replayed
		end
	end
	local _close_pending <close> = claim and claim.guard
	local ok, res = pcall(function()
		local handled, routed = route_frontend(s, "steer", text, run_opts)
		if handled then
			return routed
		end
		if not s.A.busy then
			lib.fail("agent.no_active_turn", "no active turn to steer")
		end
		if expected_turn ~= nil and expected_turn ~= s.A.turn_id then
			lib.fail("agent.turn_mismatch", "active turn changed", { turn_id = s.A.turn_id })
		end
		local failure = poison_reason()
		if failure then
			error(failure, 0)
		end
		local session_op, session_replayed = begin_session_op(s, rid, "agent.steer", fp, opts)
		local _release_op <close> = session_op and session_op.guard
		if session_replayed then
			local orig = copy_result(session_op)
			if claim then
				claim.ok(orig)
			end
			return replay_result_table(orig)
		end
		-- A steer received after cancellation belongs to the next turn.
		local cancelling = s.A.token and s.A.token:is_cancelled()
		local target_queue = cancelling and s.A.follow or s.A.steer
		local input_id, err = enqueue(s, target_queue, text, run_opts, function(eid)
			return build_session_op(session_op, { input_id = eid })
		end, function(eid)
			if session_op then
				session_op.state.committed = true
			end
			if claim then
				claim.ok({ input_id = eid })
			end
		end)
		if not input_id then
			error(err, 0)
		end
		if not s.A.busy then
			wakeup.wake_pending(s)
		end
		return { input_id = input_id }
	end)
	if not ok then
		if claim then
			claim.err(res)
		end
		error(res, 0)
	end
	if claim then
		claim.ok(res)
	end
	return res
end

local function agent_notify(s, _, _, text, opts)
	local failure = poison_reason()
	if failure then
		error(failure, 0)
	end
	s.D.inputs.notify_enqueue(text, switching(s))
	if opts and opts.wake then
		s.A.wake = true
	end
	if not s.A.busy then
		wakeup.wake_pending(s)
	end
end

local function agent_append(s, _, _, msg, opts)
	local failure = poison_reason()
	if failure then
		error(failure, 0)
	end
	if switching(s) then
		return nil, "session switch in progress; retry"
	end
	if opts and opts.operation ~= nil then
		local prepared = {}
		if msg ~= nil then
			if mare.projection.active() then
				local out, lossy = mare.projection.project(msg, { lossy = true })
				if lossy and mare.ui and mare.ui.notify then
					mare.ui.notify("session entry contained non-UTF-8 bytes; replaced lossily with U+FFFD", "warning")
				end
				msg = out
			else
				msg = mare.projection.project(msg)
			end
			msg.timestamp = msg.timestamp or mare.task.now()
			prepared[1] = msg
		end
		local ok, err = pcall(s.D.inputs.mutate, function()
			return true, false, opts.operation, prepared
		end)
		if not ok then
			return nil, tostring(err)
		end
		return true
	end
	msg = mare.projection.project(msg)
	msg.timestamp = msg.timestamp or mare.task.now()
	return s.append(msg)
end

local function agent_queue_custom(s, _, _, msg, mode, opts)
	local failure = poison_reason()
	if failure then
		error(failure, 0)
	end
	msg = copy_result(mare.projection.project(msg))
	if type(msg) == "table" then
		msg.notification = nil
	end
	if mode == "next_turn" or mode == "pending" then
		if type(msg) == "table" and msg.role == "user" then
			require("mare_lib").fail(
				"invalid_arguments",
				"user-role messages cannot use " .. tostring(mode) .. "; use steer or follow_up"
			)
		end
	end
	if mode == "next_turn" then
		table.insert(s.A.next_turn, msg)
	elseif mode == "pending" then
		table.insert(s.A.pending_custom, msg)
	elseif mode == "steer" or mode == "follow_up" then
		local queue = mode == "steer" and s.A.steer or s.A.follow
		local text = opts and type(opts.text) == "string" and opts.text or ""
		local eid, err = enqueue(s, queue, text, {
			custom_message = msg,
			operation = opts and opts.operation or nil,
		})
		if not eid then
			error(err, 0)
		end
		return eid
	else
		error("invalid custom queue mode: " .. tostring(mode), 0)
	end
	return true
end

local function restore_queued_editor(queued)
	local texts, images, markers, text_offset = {}, {}, {}, 0
	for _, q in ipairs(queued) do
		local text, shift = q.text or "", 0
		for i, image in ipairs(q.images or {}) do
			images[#images + 1] = image
			local marker = q.image_markers and q.image_markers[i]
			local label = "[Image #" .. #images .. "]"
			local offset = marker and marker.offset + shift
			if marker and text:sub(offset + 1, offset + #marker.text) == marker.text then
				text = text:sub(1, offset) .. label .. text:sub(offset + #marker.text + 1)
				markers[#images] = { text = label, offset = text_offset + offset }
				shift = shift + #label - #marker.text
			else
				markers[#images] = { text = label, offset = text_offset + #text + 1 }
				text = text .. " " .. label
			end
		end
		texts[#texts + 1] = text
		text_offset = text_offset + #text + 2
	end
	if #queued > 0 and mare.ui.connected() then
		mare.ui.set_editor(table.concat(texts, "\n\n"), { prepend = true, images = images, image_markers = markers })
	end
end

local function agent_cancel(s, _, _, opts)
	opts = opts or {}
	local rid = validate_request_id(opts)
	local turn_id = type(opts.turn_id) == "string" and opts.turn_id ~= "" and opts.turn_id or nil
	if opts.turn_id ~= nil and turn_id == nil then
		require("mare_lib").fail("invalid_arguments", "turn_id must be a non-empty string")
	end
	local mode = opts.mode or "cancel"
	if mode ~= "cancel" and mode ~= "halt" then
		require("mare_lib").fail("invalid_arguments", "mode must be `cancel` or `halt`")
	end
	if mode == "cancel" then
		local handled, routed = route_frontend(s, "cancel", nil, opts)
		if handled then
			return routed
		end
	end
	local clear_queue = mode == "halt" and opts.clear_queue == true or (mode == "cancel" and opts.clear_queue ~= false)
	local skey, fp
	if rid then
		skey = session_receipt_key(s)
		fp = mare.crypto.sha256(mare.json.encode({
			turn_id = turn_id or false,
			clear_queue = clear_queue,
			mode = mode,
			reason = opts.reason or false,
		}))
		local replayed = settled_receipt(s.cancel_receipts, skey, rid, fp)
		if replayed then
			return replayed
		end
	end
	local session = s.D.follow.session()
	local info = session and session:info()
	local session_op, session_replayed
	if rid and poison_reason() == nil and not (info and info.readonly) then
		session_op, session_replayed = begin_session_op(s, rid, "agent.cancel", fp, opts)
	end
	local _release_op <close> = session_op and session_op.guard
	if session_replayed then
		local orig = copy_result(session_op)
		record_receipt(s.cancel_receipts, skey, rid, fp, orig)
		return replay_result_table(orig)
	end
	local function finish(res)
		if rid then
			record_receipt(s.cancel_receipts, skey, rid, fp, res)
		end
		return res
	end

	local res_turn_id = nil
	local should_drain = false

	if mode == "halt" then
		if s.A.busy and (turn_id == nil or turn_id == s.A.turn_id) then
			s.A.halt = s.A.halt or tostring(opts.reason or "halted")
		end
		should_drain = clear_queue
	else
		local turn_mismatch = turn_id ~= nil and (not s.A.busy or turn_id ~= s.A.turn_id)
		if not turn_mismatch then
			local compact = s.D.follow and s.D.follow.compact and s.D.follow.compact()
			if
				compact
				and require("mare_lib").callable(compact.busy)
				and compact:busy()
				and require("mare_lib").callable(compact.cancel)
			then
				pcall(compact.cancel, compact)
			end
			if s.A.busy then
				res_turn_id = s.A.turn_id
				-- Cancelling active work is independent of whether the session barrier or reload gate is open.
				if s.A.token then
					if not s.A.token:is_cancelled() then
						local reason = { source = "user" }
						local prefix = "mare.cancel-cause:"
						if
							mare.mode == "child"
							and type(opts.reason) == "string"
							and opts.reason:sub(1, #prefix) == prefix
						then
							local ok, cause = pcall(mare.json.decode, opts.reason:sub(#prefix + 1))
							if ok and type(cause) == "table" and type(cause.source) == "string" then
								reason = cause
							end
						elseif
							opts.reason == "user"
							or opts.reason == "parent"
							or opts.reason == "hook"
							or opts.reason == "disposed"
						then
							reason = { source = opts.reason }
						elseif opts.reason then
							reason.detail = opts.reason
						end
						s.A.cancel_reason = reason
					end
					if not clear_queue and not s.A.token:is_cancelled() then
						-- A retained pre-cancel inbox is visible but does not itself restart work.
						-- Inputs accepted after cancellation remain wakeable.
						wakeup.park_existing_inputs(s)
					end
					s.A.token:cancel()
				end
			end
			should_drain = clear_queue
		end
	end

	local res
	if poison_reason() ~= nil then
		local removed_items = {}
		if should_drain and (#s.A.steer > 0 or #s.A.follow > 0) then
			local entries, deliveries = s.D.inputs.drain_in_memory()
			if #entries > 0 then
				restore_queued_editor(entries)
				removed_items = s.D.queue_view_items(entries, deliveries, true)
			end
		end
		res = { removed = mare.json.array(removed_items) }
		if res_turn_id ~= nil then
			res.turn_id = res_turn_id
		end
	elseif session_op ~= nil or (should_drain and (#s.A.steer > 0 or #s.A.follow > 0)) then
		local out = s.D.inputs.mutate(function(q)
			local e, d = {}, {}
			if should_drain and (#q.steer > 0 or #q.follow_up > 0) then
				e, d = s.D.inputs.drain_entries(q)
			end
			local items = #e > 0 and s.D.queue_view_items(e, d, true) or {}
			local result = { removed = mare.json.array(items) }
			if res_turn_id ~= nil then
				result.turn_id = res_turn_id
			end
			local op = build_session_op(session_op, result)
			return { entries = e, result = result }, #e > 0, op
		end)
		if session_op then
			session_op.state.committed = true
		end
		if #out.entries > 0 then
			restore_queued_editor(out.entries)
		end
		res = out.result
	else
		res = { removed = mare.json.array() }
		if res_turn_id ~= nil then
			res.turn_id = res_turn_id
		end
	end

	return finish(res)
end

local function agent_wait(s, _, _, turn_id, opts)
	if type(turn_id) == "table" and opts == nil then
		opts = turn_id
		turn_id = opts.turn_id
	end
	opts = opts or {}
	local function live_for_current_session(id)
		local entry = id and s.turns_live[id] or nil
		if entry and entry.session_gen == s.turn_session_gen and entry.session_id == s.A.session_id then
			return entry
		end
		return nil
	end
	local has_main = turn_id ~= nil
		and turn_id ~= ""
		and (s.turns[turn_id] ~= nil or live_for_current_session(turn_id) ~= nil)
	if not has_main then
		local handled, routed = route_frontend(s, "wait", nil, {
			turn_id = turn_id,
			timeout_ms = opts.timeout_ms,
		})
		if handled then
			return routed
		end
	end
	local target_id = turn_id or (s.A.busy and s.A.turn_id) or current_settling_turn(s) or s.last_turn_id
	if not target_id or target_id == "" then
		require("mare_lib").fail("invalid_arguments", "no active or completed turn to wait on")
	end
	local settled = s.turns[target_id]
	if settled then
		return settled
	end
	local live = live_for_current_session(target_id)
	if not live then
		require("mare_lib").fail("invalid_arguments", "unknown turn_id: " .. tostring(target_id))
	end
	local tx, rx = mare.task.channel()
	live.waiters[#live.waiters + 1] = tx
	-- Re-check after registering in case the turn completed right as we registered.
	if s.turns[target_id] then
		return s.turns[target_id]
	end
	if type(opts.timeout_ms) == "number" then
		local ok, rec = mare.task.timeout(math.floor(opts.timeout_ms), function()
			return rx:recv()
		end)
		if not ok then
			return {
				turn_id = target_id,
				status = "running",
				timed_out = true,
				started_at_ms = live.started_at_ms,
			}
		end
		return rec
	end
	return rx:recv()
end

return function(D)
	local validate = require("agent.service_validate")(D.ctx)
	D.validate = validate
	local s = {
		D = D,
		ctx = D.ctx,
		cfg = D.cfg,
		A = D.A,
		disposed = false,
		parked_inputs = nil,
		settling = 0,
		settling_turns = {},
		turn_session_gen = 0,
		turns_live = {},
		turns = {},
		turn_order = {},
		last_turn_id = nil,
		prompt_receipts = {},
		prompt_pending = {},
		steer_receipts = {},
		steer_pending = {},
		cancel_receipts = {},
		enqueue_receipts = {},
		-- forwarders: persist, inputs and loop load on their first call, not while wiring here
		append = function(...)
			return D.persist.append(...)
		end,
		persist = function(...)
			return D.persist.persist(...)
		end,
		orphans = function(...)
			return D.persist.orphans(...)
		end,
		drain = function(...)
			return D.inputs.drain(...)
		end,
		build_user = function(...)
			return D.inputs.build_user(...)
		end,
		note_message = function(...)
			return D.inputs.note_message(...)
		end,
		take_notes = function(...)
			return D.inputs.take_notes(...)
		end,
		entry = function(...)
			return D.inputs.entry(...)
		end,
		run = function(...)
			return D.loop.run(...)
		end,
		switching = switching,
		compacting = compacting,
		complete_run = complete_run,
		publish_end = publish_end,
		register_turn_live = register_turn_live,
		claim_prompt = claim_prompt,
		finish_prompt = finish_prompt,
		normalize_input = validate.normalize_input,
		check_media_support = validate.check_media_support,
		validate_output_schema = validate.validate_output_schema,
	}
	D.on_consume_inputs = function(ids)
		if s.parked_inputs then
			for _, id in ipairs(ids or {}) do
				s.parked_inputs[id] = nil
			end
		end
	end
	D.is_input_parked = function(id)
		return s.parked_inputs ~= nil and id ~= nil and s.parked_inputs[id] == true
	end

	local ENQUEUE_PUBLIC_KEYS = {
		cancel = true,
		delivery = true,
		source = true,
		input_context = true,
		request_id = true,
		images = true,
		image_markers = true,
		attachments = true,
	}
	local function pick_enqueue_opts(raw_opts)
		if raw_opts == nil then
			return {}
		end
		local lib = require("mare_lib")
		if type(raw_opts) ~= "table" then
			lib.fail("invalid_arguments", "enqueue options must be a table")
		end
		if raw_opts.custom_message ~= nil or raw_opts.notification ~= nil then
			lib.fail("invalid_arguments", "enqueue does not accept internal message classification fields")
		end
		local out = {}
		for k in pairs(ENQUEUE_PUBLIC_KEYS) do
			if raw_opts[k] ~= nil then
				out[k] = raw_opts[k]
			end
		end
		return out
	end
	local agent = {}
	function agent.capabilities(_, _)
		return { input_admission = 1 }
	end
	function agent.enqueue(_, _, raw_input, owner, raw_opts)
		local lib = require("mare_lib")
		local opts = pick_enqueue_opts(raw_opts)
		local rid = validate_request_id(opts)
		local delivery = opts.delivery or "follow_up"
		if delivery ~= "follow_up" and delivery ~= "steer" then
			lib.fail("invalid_arguments", "delivery must be `follow_up` or `steer`")
		end
		local text, images, image_markers, content = s.normalize_input(raw_input, opts)
		if text:match("^%s*$") and not (images and #images > 0) and not (content and #content > 0) then
			lib.fail("invalid_arguments", "enqueue input must not be empty")
		end
		local run_opts = {}
		for k, v in pairs(opts) do
			run_opts[k] = v
		end
		run_opts.images = images and lib.copy_deep(images) or nil
		run_opts.image_markers = image_markers and lib.copy_deep(image_markers) or nil
		run_opts.content = content and lib.copy_deep(content) or nil
		if opts.input_context ~= nil then
			run_opts.input_context = mare.projection.project(copy_result(opts.input_context))
		end
		local media_err, media_cap = s.check_media_support(run_opts)
		if media_err then
			lib.fail("unsupported", media_err, { capability = media_cap })
		end
		local sess, info, ready_err, ready_status = writable_session_state(s, true, false)
		if not info then
			return { status = ready_status, durable = false, error = ready_err }
		end
		if
			type(owner) ~= "table"
			or owner.session_id ~= info.session_id
			or owner.session_epoch ~= info.session_epoch
		then
			return {
				status = "stale",
				durable = false,
				error = "active session changed before enqueue",
				session_id = info.session_id,
				session_epoch = info.session_epoch,
			}
		end
		local failure = poison_reason()
		if failure then
			return { status = "rejected", durable = false, error = tostring(failure) }
		end
		if switching(s) then
			return { status = "rejected", durable = false, error = "session switch in progress; retry" }
		end
		if opts.cancel and opts.cancel:is_cancelled() then
			return { status = "cancelled", durable = false, error = "cancelled" }
		end
		local function post_claim_ready()
			local post_sess, post_info, post_err, post_status = writable_session_state(s, true, false)
			if not post_info then
				return nil, nil, { status = post_status, durable = false, error = post_err }
			end
			if not rawequal(post_sess, sess) then
				return nil,
					nil,
					{
						status = "stale",
						durable = false,
						error = "session provider changed before enqueue",
						session_id = post_info.session_id,
						session_epoch = post_info.session_epoch,
					}
			end
			if owner.session_id ~= post_info.session_id or owner.session_epoch ~= post_info.session_epoch then
				return nil,
					nil,
					{
						status = "stale",
						durable = false,
						error = "active session changed before enqueue",
						session_id = post_info.session_id,
						session_epoch = post_info.session_epoch,
					}
			end
			local post_failure = poison_reason()
			if post_failure then
				return nil, nil, { status = "rejected", durable = false, error = tostring(post_failure) }
			end
			if switching(s) then
				return nil, nil, { status = "rejected", durable = false, error = "session switch in progress; retry" }
			end
			if opts.cancel and opts.cancel:is_cancelled() then
				return nil, nil, { status = "cancelled", durable = false, error = "cancelled" }
			end
			return post_sess, post_info, nil
		end
		local skey, fp, cached_replay
		if rid then
			if not (sess and sess.claim_operation) then
				return {
					status = "rejected",
					durable = false,
					error = "session provider does not support atomic idempotency receipts",
				}
			end
			skey = tostring(info.session_id or s.A.session_id or "")
			fp = mare.crypto.sha256(mare.json.encode_strict({
				session_id = info.session_id,
				delivery = delivery,
				text = text,
				images = run_opts.images or false,
				image_markers = run_opts.image_markers or false,
				content = run_opts.content or false,
				source = opts.source or false,
				input_context_present = opts.input_context ~= nil,
				input_context = opts.input_context ~= nil and run_opts.input_context or false,
			}))
			cached_replay = settled_receipt(s.enqueue_receipts, skey, rid, fp)
			if cached_replay then
				local _, _, gate_err = post_claim_ready()
				if gate_err then
					return gate_err
				end
				return cached_replay
			end
		end
		local session_op, session_replayed, op_err = begin_session_op(s, rid, "agent.enqueue", fp, {
			cancel = opts.cancel,
			return_typed_error = true,
		}, nil, owner, sess)
		if op_err then
			if op_err.code == "cancelled" then
				return { status = "cancelled", durable = false, error = "cancelled" }
			elseif op_err.code == "stale_session" then
				return { status = "stale", durable = false, error = tostring(op_err) }
			end
			return { status = "rejected", durable = false, error = tostring(op_err) }
		end
		local _release_op <close> = session_op and session_op.guard
		local post_sess, post_info, gate_err = post_claim_ready()
		if gate_err then
			return gate_err
		end
		if session_replayed then
			local orig = copy_result(session_op)
			if rid then
				record_receipt(s.enqueue_receipts, skey, rid, fp, orig)
			end
			return replay_result_table(orig)
		end
		local target_queue = delivery == "steer" and s.A.steer or s.A.follow
		local committed_receipt
		local input_id, err, attempted_eid = enqueue(s, target_queue, text, run_opts, function(eid)
			committed_receipt = {
				status = "queued",
				durable = true,
				input_id = eid,
				session_id = post_info.session_id,
				session_epoch = post_info.session_epoch,
			}
			return build_session_op(session_op, committed_receipt)
		end, function(eid)
			if session_op then
				session_op.state.committed = true
			end
			wakeup.park_input(s, eid)
			if rid then
				record_receipt(s.enqueue_receipts, skey, rid, fp, committed_receipt)
			end
		end, owner, post_sess)
		if not input_id then
			local err_code = type(err) == "table" and err.code or nil
			local err_phase = type(err) == "table" and err.phase or nil
			if
				rid
				and (err_phase == "committed" or err_code == "post_commit_failed" or err_code == "missing_operation_receipt" or err_code == "cancelled" or (opts.cancel and opts.cancel:is_cancelled()))
				and type(post_sess.operation_receipt) == "function"
			then
				local ok_rec, rec_res, rec_err, rec_code = pcall(post_sess.operation_receipt, post_sess, {
					request_id = rid,
					kind = "agent.enqueue",
					fingerprint = fp,
					session_id = owner.session_id,
					session_epoch = owner.session_epoch,
				})
				if
					ok_rec
					and type(rec_res) == "table"
					and rec_res.status == "replayed"
					and valid_receipt_result(rec_res.receipt, rid, "agent.enqueue", fp)
				then
					local orig = copy_result(rec_res.receipt.result)
					wakeup.park_input(s, orig.input_id)
					record_receipt(s.enqueue_receipts, skey, rid, fp, orig)
					return orig
				elseif ok_rec and rec_code == "conflict" then
					lib.fail(
						"request_id_conflict",
						tostring(rec_err or ("request_id %q was already used for a different payload"):format(rid))
					)
				end
			end
			if
				err_phase == "committed"
				or err_code == "post_commit_failed"
				or err_code == "missing_operation_receipt"
			then
				return {
					status = "indeterminate",
					input_id = attempted_eid,
					error = tostring(err),
				}
			end
			if err_code == "cancelled" or (opts.cancel and opts.cancel:is_cancelled()) then
				return { status = "cancelled", durable = false, error = "cancelled" }
			end
			if err_code == "stale_session" then
				return { status = "stale", durable = false, error = tostring(err) }
			end
			return { status = "rejected", durable = false, error = tostring(err) }
		end
		return committed_receipt
	end
	function agent.resume_input(_, _, owner, opts)
		local lib = require("mare_lib")
		if opts ~= nil and type(opts) ~= "table" then
			lib.fail("invalid_arguments", "resume_input options must be a table")
		end
		opts = opts or {}
		if opts.input_id ~= nil and (type(opts.input_id) ~= "string" or opts.input_id == "") then
			lib.fail("invalid_arguments", "input_id must be a non-empty string")
		end
		for _ = 1, 4 do
			local gen = s.A.run_id
			local seq = s.A.session_seq
			local sess, info, pre_err, pre_status = writable_session_state(s, true, true)
			if not info then
				return { status = pre_status, error = pre_err }
			end
			if
				type(owner) ~= "table"
				or owner.session_id ~= info.session_id
				or owner.session_epoch ~= info.session_epoch
			then
				return { status = "stale", error = "session owner mismatch" }
			end
			if s.A.busy then
				return { status = "busy", error = "agent is busy" }
			end
			local sw_busy = switching(s)
			local cmp_busy = compacting(s)
			if sw_busy or cmp_busy then
				return { status = "busy", error = "compaction or switch in progress" }
			end
			local failure = poison_reason()
			if failure then
				return { status = "rejected", error = tostring(failure) }
			end
			if s.A.busy then
				return { status = "busy", error = "agent is busy" }
			end
			local cur_sess, cur_info, cur_err, cur_status
			local ok_sync, sync_err = pcall(mare.task.call_sync, function()
				cur_sess, cur_info, cur_err, cur_status = writable_session_state(s, true, true)
				if not cur_info then
					return
				end
				if cur_sess and lib.callable(cur_sess.switching) and cur_sess:switching() then
					cur_err, cur_status = "compaction or switch in progress", "busy"
				end
				local compact = s.D.follow and s.D.follow.compact and s.D.follow.compact()
				if not cur_err and compact and lib.callable(compact.busy) and compact:busy() then
					cur_err, cur_status = "compaction or switch in progress", "busy"
				end
			end)
			if not ok_sync then
				return { status = "busy", error = tostring(sync_err or "session state check failed") }
			end
			if cur_err then
				return { status = cur_status, error = cur_err }
			end
			if
				not cur_info
				or owner.session_id ~= cur_info.session_id
				or owner.session_epoch ~= cur_info.session_epoch
			then
				return { status = "stale", error = "session owner mismatch" }
			end
			if
				not s.A.busy
				and s.A.run_id == gen
				and s.A.session_seq == seq
				and cur_sess ~= nil
				and rawequal(cur_sess, sess)
			then
				local target_id
				if opts.input_id then
					for _, queue in ipairs({ s.A.steer, s.A.follow }) do
						for _, item in ipairs(queue or {}) do
							if item.input_id == opts.input_id then
								target_id = item.input_id
								break
							end
						end
						if target_id then
							break
						end
					end
					if not target_id then
						return { status = "empty", error = "queued input not found: " .. tostring(opts.input_id) }
					end
					if s.parked_inputs then
						s.parked_inputs[target_id] = nil
					end
				else
					local head = (s.A.steer and s.A.steer[1]) or (s.A.follow and s.A.follow[1])
					if not head or not head.input_id then
						return { status = "empty", error = "no queued inputs to resume" }
					end
					target_id = head.input_id
					s.parked_inputs = nil
				end
				local run_opts = {
					queued = true,
					input_id = opts.input_id,
					skip_parked = opts.input_id ~= nil or nil,
				}
				local turn_id = claim_prompt(s, run_opts)
				if s.ctx and s.ctx.kernel and s.ctx.kernel.mark_call_committed then
					s.ctx.kernel:mark_call_committed()
				end
				s.ctx:spawn(function()
					finish_prompt(s, nil, run_opts, turn_id)
				end)
				return { status = "started", turn_id = turn_id, input_id = target_id }
			end
		end
		return { status = "busy", error = "agent state changed while resuming" }
	end
	function agent.register_activity(_, caller, name, provider)
		return D.activity.register(caller, name, provider)
	end
	function agent.activity(_, _)
		return D.activity.snapshot()
	end
	--- step_participant(name, prepare, opts?) -> remove: stage per-step state (and, with
	--- output = "transaction", disclosures) committed atomically before each model request.
	function agent.step_participant(_, caller, name, prepare, opts)
		return D.steps.register(caller, name, prepare, opts)
	end
	function agent.step_state(_, _, name)
		return D.steps.state(name)
	end
	function agent.step_preview(_, _, name, step_id)
		return D.steps.preview(name, step_id)
	end
	--- prompt(text, opts) -> {status, turn_id?, input_id?, ...}
	--- opts: {when_busy=, request_id=, expected_session=, output_schema=, system=, tools=, source=, effort=, ...}
	function agent.prompt(self, caller, raw_input, opts)
		local clean_opts = opts
		if type(opts) == "table" then
			clean_opts = {}
			for k, v in pairs(opts) do
				if k ~= "internal_notification" and k ~= "notification" then
					clean_opts[k] = v
				end
			end
		end
		return agent_prompt(s, self, caller, raw_input, clean_opts)
	end
	--- interrupted(): true when the context ends mid-turn (last message is a user message,
	--- a tool result, or an assistant message with unanswered tool calls, e.g. after a crash)
	--- — the loop can continue without a new prompt
	function agent.interrupted(_, _)
		local last = s.A.messages[#s.A.messages]
		-- the user stopped that turn on purpose: nothing to continue
		if last and last.turn_aborted then
			return false
		end
		return last ~= nil
			and (last.role == "user" or last.role == "tool_result" or last.role == "custom" or has_tool_call(last))
	end
	--- continue(): resume an interrupted turn from the existing context;
	--- unfinished tool calls get synthesized "aborted" results by the request normalization
	function agent.continue(self, caller, opts)
		return agent_continue(s, self, caller, opts)
	end
	function agent.steer(self, caller, raw_input, opts)
		return agent_steer(s, self, caller, raw_input, opts)
	end
	function agent.cancel(self, caller, opts)
		return agent_cancel(s, self, caller, opts)
	end
	function agent.wait(self, caller, turn_id, opts)
		return agent_wait(s, self, caller, turn_id, opts)
	end
	--- notify(text, {wake=bool}): deliver a notification to the model. While busy it joins the
	--- next step (several notifications share one message); when idle and wake = true it
	--- starts a new turn, otherwise it waits for the next prompt.
	function agent.notify(self, caller, text, opts)
		return agent_notify(s, self, caller, text, opts)
	end
	function D.notify_output(text, opts)
		if s.disposed then
			return false, "active_output_unavailable"
		end
		agent_notify(s, nil, nil, text, opts)
		return true
	end
	--- append a message to the context without running the model (e.g. `!cmd` output)
	function agent.append(self, caller, msg, opts)
		return agent_append(s, self, caller, msg, opts)
	end
	--- queue_custom(msg, mode, opts?): queue a custom message for `"next_turn"`, `"pending"`, `"steer"`, or `"follow_up"`
	function agent.queue_custom(self, caller, msg, mode, opts)
		return agent_queue_custom(s, self, caller, msg, mode, opts)
	end
	function agent.list_messages(_, _, opts)
		local projected = project_messages(s.A.messages, { preserve_invalid = true }) or {}
		if type(opts) == "table" and (opts.cursor ~= nil or opts.limit ~= nil) then
			return require("mare_lib").paginate(projected, opts)
		end
		return { data = mare.json.array(projected) }
	end
	function agent.messages(_, _)
		return s.A.messages
	end
	function agent.baseline_cut(_, _)
		local items = s.D.activity.baseline_items(true)
		local skeletons = s.D.activity.baseline_items(false)
		local function project_queue(q, delivery)
			if type(s.D.queue_view_items) == "function" then
				local deliveries = {}
				for i = 1, #(q or {}) do
					deliveries[i] = delivery
				end
				return mare.json.array(s.D.queue_view_items(q or {}, deliveries))
			end
			return mare.json.array()
		end
		local steer = project_queue(s.A.steer, "steer")
		local follow_up = project_queue(s.A.follow, "follow_up")
		local page_slice = require("mare_lib.baseline_page")
		return {
			retained = {
				state = s.A.busy and "running" or "idle",
				queue = { steer = steer, follow_up = follow_up },
				active_turn = s.A.busy and { turn_id = s.A.turn_id } or nil,
				active_items = mare.json.array(skeletons),
			},
			parts = {
				{
					name = "active_items",
					page = function(cursor, max_bytes, limit)
						return page_slice(items, #items, cursor, max_bytes, limit)
					end,
				},
			},
		}
	end

	s.ctx:on("session/switch-completed", function()
		wakeup.wake_pending(s)
	end)
	s.ctx:on("session/deleted", function(ev)
		if ev and type(ev.session_id) == "string" and ev.session_id ~= "" then
			local skey = ev.session_id
			s.prompt_receipts[skey] = nil
			s.prompt_pending[skey] = nil
			s.steer_receipts[skey] = nil
			s.steer_pending[skey] = nil
			s.cancel_receipts[skey] = nil
			s.enqueue_receipts[skey] = nil
		end
	end)
	-- compaction finished: the built-in compact signals every release (`compact/idle`, which
	-- covers summarize_only and early exits); a replacement compact service signals through the
	-- public `compact/completed`. Waking is idempotent, so both arriving runs the pending work once.
	s.ctx:on("compact/completed", function()
		wakeup.wake_pending(s)
	end)
	s.ctx:on("compact/idle", function()
		wakeup.wake_pending(s)
	end)
	-- A disposed session can no longer publish switch-complete. Retry once a replacement
	-- service is installed, after its own state is ready.
	s.ctx:inject({ "session" }, function()
		wakeup.wake_pending(s)
	end)

	require("agent.service_session")({
		ctx = s.ctx,
		cfg = s.cfg,
		A = s.A,
		persist = s.persist,
		D = s.D,
		agent = agent,
		on_restore = function()
			wakeup.park_existing_inputs(s)
		end,
		on_session_change = function()
			reset_turn_cache(s)
		end,
		route_frontend = function(op, text, opts)
			return route_frontend(s, op, text, opts)
		end,
	})
	require("agent.service_events")({
		ctx = s.ctx,
		A = s.A,
		orphans = s.orphans,
		agent = agent,
		on_dispose = function()
			s.disposed = true
		end,
	})
end
