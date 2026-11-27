-- session.store_switch: turn/switch lease coordination, side-request draining,
-- snapshot/revision capture, and atomic session publication;
-- helper submodule of `session.store`.
local session_guard, poison, poison_reason, session_spawn =
	mare.task.publish, mare.task.fail_session, mare.task.session_error, mare.task.spawn_completion
local call_sync = mare.task.call_sync
local table_revision_mod
local function table_revision()
	if not table_revision_mod then
		table_revision_mod = require("mare_lib.table_revision")
	end
	return table_revision_mod
end
local function one_line_err(err)
	return require("session.load_helpers").one_line_err(err)
end

local function turn_active(st)
	return st.turns_in_flight > 0 or st.ctx.agent:activity().busy
end

-- All public session switches share this guard, including RPC and plugin callers.
local function require_idle(st)
	if st.is_retired and st.is_retired() then
		require("mare_lib").fail("busy", "Cannot switch sessions after session writer retirement")
	end
	if st.has_active_writes and st.has_active_writes() then
		require("mare_lib").fail("busy", "Cannot switch sessions while a session write is in progress")
	end
	if turn_active(st) then
		require("mare_lib").fail("busy", "Cannot switch sessions while a turn is running or ending")
	end
	local failure = st.ctx.agent:persist_error()
	if failure then
		require("mare_lib").fail(
			"persist_failed",
			"Cannot switch sessions while persistence records are pending: " .. tostring(failure)
		)
	end
end

local function switching(st)
	return st.switch_lease ~= nil or poison_reason() ~= nil
end

local function is_transferred(st)
	return not not (st.switch_lease and st.switch_lease.transferred)
end

local function completion_token(st)
	local lease = st.switch_lease
	local tid = mare.task.id()
	if
		lease
		and lease.transferred
		and lease.completion_token ~= nil
		and lease.completion_task ~= nil
		and tid == lease.completion_task
	then
		return lease.completion_token
	end
	return nil
end

local function matches_completion(st, token)
	local lease = st.switch_lease
	return token ~= nil
		and lease ~= nil
		and lease.transferred == true
		and rawequal(lease.completion_token, token)
		and lease.completion_task ~= nil
		and mare.task.id() == lease.completion_task
end

local function release_switch(st, lease)
	if st.switch_lease ~= lease then
		return
	end
	st.switch_lease = nil
	if poison_reason() then
		return
	end
	pcall(st.ctx.spawn, st.ctx, function()
		st.ctx.bus:emit("session/switch-completed", { committed = lease.committed == true })
	end)
end

local function begin_switch(st)
	local failure = poison_reason()
	if failure then
		error(failure, 0)
	end
	if st.disposed then
		error("Cannot switch sessions: session service was disposed", 0)
	end
	if st.is_retired and st.is_retired() then
		error("Cannot switch sessions after session writer retirement", 0)
	end
	if mare.exec.current() then
		error("Cannot switch sessions from inside a tool call", 0)
	end
	if st.switch_lease then
		require("mare_lib").fail("busy", "Cannot switch sessions while another switch is in progress; retry")
	end
	local lease = {}
	st.switch_lease = lease
	return setmetatable(lease, {
		__close = function(self)
			if not self.transferred then
				if self.identity_switch then
					pcall(self.identity_switch.abort, self.identity_switch)
					self.identity_switch = nil
				end
				if self.stage then
					pcall(self.stage.close, self.stage)
				end
				if self.target then
					pcall(self.discard_target and self.target.discard or self.target.close, self.target)
				end
				release_switch(st, self)
			end
		end,
	})
end

local function poison_interrupted_completion(st)
	if st.switch_lease and st.switch_lease.transferred then
		poison("session completion was interrupted by service disposal")
	end
end

-- A switch must not close the writer while an auxiliary request can still
-- report usage. Non-yieldable callers fail rather than switch unsafely.
local function requests_pending(st, owner)
	local llm = st.D.follow.llm()
	local compact = st.D.follow.compact()
	return (llm and require("mare_lib").callable(llm.side_requests_pending) and llm:side_requests_pending(owner))
		or (compact and require("mare_lib").callable(compact.busy) and compact:busy())
end

local function drain_side(st, wait_turn, expected_owner)
	local llm = st.D.follow.llm()
	local compact = st.D.follow.compact()
	local owner = expected_owner or st.S.id
	if st.S.id ~= owner then
		return nil, "Could not start a new session: the active session changed"
	end
	local function side_pending()
		return llm and require("mare_lib").callable(llm.side_requests_pending) and llm:side_requests_pending(owner)
	end
	if side_pending() then
		if not require("mare_lib").callable(llm.cancel_side_requests) then
			return nil, "Cannot switch sessions: side requests cannot be cancelled"
		end
		llm:cancel_side_requests(owner)
	end
	if compact and require("mare_lib").callable(compact.busy) and compact:busy() then
		if not require("mare_lib").callable(compact.cancel) then
			return nil, "Cannot switch sessions: compaction cannot be cancelled"
		end
		compact:cancel()
	end
	local function pending()
		return (wait_turn and wait_turn()) or requests_pending(st, owner)
	end
	if not pending() then
		return true
	end
	if not coroutine.isyieldable() then
		return nil, "Cannot switch sessions: requests are still stopping; try again"
	end
	local deadline = mare.task.clock() + 1000
	while pending() do
		if st.S.id ~= owner then
			return nil, "Could not start a new session: the active session changed"
		end
		if mare.task.clock() >= deadline then
			if wait_turn and wait_turn() then
				return nil, "Could not start a new session: the previous turn did not stop in time"
			end
			return nil, "Cannot switch sessions: side requests did not stop in time"
		end
		mare.task.sleep(10)
	end
	if st.S.id ~= owner then
		return nil, "Could not start a new session: the active session changed"
	end
	return true
end

-- Child sessions use the host-assigned public session id when initialized.
local function new_session_id()
	return (mare.mode == "child" and mare.args and mare.args.session_id) or mare.crypto.uuid()
end

-- A detached state for a pending load. No live session or agent state is touched.
local function new_snapshot(st, cwd, id)
	id = (type(id) == "string" and id ~= "") and id or mare.crypto.uuid()
	local target_dir = cwd and mare.store.dir(cwd) or st.dir
	return {
		id = id,
		generation = mare.crypto.uuid(),
		cwd = cwd,
		created_at_ms = require("mare_lib").to_epoch_ms(mare.task.now()),
		planned = require("mare_lib").session_file_path(target_dir, id),
		entries = {},
		known_ids = {},
		list = {},
		operations = {},
		labels = {},
		tool_states = {},
		model_recs = {},
		prunes = {},
		ids = setmetatable({}, { __mode = "k" }),
		partial = false,
		msg_count = 0,
	}
end

local function revision(st)
	return {
		id = st.S.id,
		leaf = st.S.leaf,
		count = #st.S.list,
		file = st.S.file,
		state = table_revision().snapshot(st.S),
		agent_provider = st.ctx.agent,
		agent = session_guard(function()
			local token, err = st.ctx.agent:session_revision()
			if (token == nil or token == false) and err ~= nil then
				error(tostring(err), 0)
			end
			return token
		end),
	}
end

local function source_matches(lease)
	return not lease.source_file
		or (
			lease.source_file:matches_path(lease.source_path)
			and (not lease.source_revision or mare.store.snapshot_matches(lease.source_file, lease.source_revision))
		)
end

local function target_matches(lease, snapshot, target)
	return not target
		or (
			snapshot.path
			and target:matches_path(snapshot.path)
			and (not lease.target_revision or mare.store.snapshot_matches(target, lease.target_revision))
		)
end

local function verify_agent_revision(st, agent, rev)
	-- Provider lookup and revision checking can execute arbitrary Lua. A throw or
	-- yield here is ambiguous, so the Rust guard poisons before returning it.
	local ok, valid = pcall(session_guard, function()
		local valid, err = agent:check_session_revision(rev.agent)
		if type(valid) ~= "boolean" or (valid == false and err ~= nil) then
			error(tostring(err or "invalid agent revision result"), 0)
		end
		return valid
	end)
	if not ok then
		error("agent revision check failed or yielded: " .. tostring(valid), 0)
	end
	if not valid then
		require("mare_lib").fail("stale_session", "Cannot switch sessions: agent context changed")
	end
end

local function activate_and_adopt(st, lease, snapshot, rev, messages, agent)
	local target = snapshot.file
	local activated, activation_err = pcall(session_guard, function()
		if target then
			st.checked_call(target.activate, target)
		end
	end)
	if not activated then
		local restored, restore_err = true, nil
		if st.S.file then
			restored, restore_err = pcall(st.checked_call, st.S.file.activate, st.S.file)
		end
		error(
			"target journal activation failed: "
				.. one_line_err(activation_err)
				.. (not restored and "; previous journal restore failed: " .. one_line_err(restore_err) or ""),
			0
		)
	end
	local adopted, err = pcall(session_guard, function()
		local adopt = agent.adopt_session
		if not require("mare_lib").callable(adopt) then
			error("agent service does not support atomic session publication", 0)
		end
		if not rawequal(st.ctx.agent, agent) or not table_revision().unchanged(rev.state) then
			error("the active session or agent provider changed during publication", 0)
		end
		st.checked_call(adopt, agent, {
			session_id = snapshot.id,
			messages = messages,
			tool_states = snapshot.tool_states,
			queue = snapshot.queue,
			revision = rev.agent,
		})
		if not target_matches(lease, snapshot, target) or not source_matches(lease) then
			error("the source or target file changed during agent publication", 0)
		end
	end)
	if not adopted then
		local restored, restore_err = true, nil
		if st.S.file then
			restored, restore_err = pcall(st.checked_call, st.S.file.activate, st.S.file)
		end
		error(
			"agent session publication failed or yielded: "
				.. tostring(err)
				.. (not restored and "; previous journal restore failed: " .. tostring(restore_err) or ""),
			0
		)
	end
end

local function retire_deferred_old(st, lease, deferred_old)
	local checked, bound = pcall(function()
		return deferred_old:matches_path(deferred_old:path())
	end)
	local marked, marker_err, retire
	if not checked or type(bound) ~= "boolean" then
		marked = false
		marker_err = "old session binding could not be verified: " .. tostring(bound)
		retire = deferred_old.discard
	elseif not bound then
		-- The validated snapshot has already been installed. A detached writer
		-- cannot publish another marker or checkpoint at its former path.
		marked, retire = true, deferred_old.discard
		lease.retirement_warning =
			"Previous session file or lock changed externally; continuing from the captured snapshot"
	else
		marked, marker_err = pcall(
			st.checked_call,
			deferred_old.append,
			deferred_old,
			{ type = "state", state = "closed", created_at_ms = require("mare_lib").to_epoch_ms(mare.task.now()) },
			{ sync = true }
		)
		retire = deferred_old.close
	end
	local retired, retire_err = pcall(st.checked_call, retire, deferred_old)
	if not marked or not retired then
		local fallback_error
		if not retired then
			local discarded, err = pcall(function()
				st.checked_call(deferred_old.discard, deferred_old)
			end)
			if not discarded then
				fallback_error = err
			end
		end
		local err = not marked and marker_err or retire_err
		local message = "session publication failed: old session close failed: " .. one_line_err(err)
		if fallback_error then
			message = message .. "; fallback discard failed: " .. one_line_err(fallback_error)
		end
		poison(message)
		st.S.close_failed = true
		error(message, 0)
	end
end

local function verify_revision(st, lease, rev)
	if st.disposed or st.switch_lease ~= lease or (st.is_retired and st.is_retired()) then
		error("Cannot switch sessions: session service was disposed", 0)
	end
	require_idle(st)
	if
		st.S.id ~= rev.id
		or st.S.leaf ~= rev.leaf
		or #st.S.list ~= rev.count
		or st.S.file ~= rev.file
		or not table_revision().unchanged(rev.state)
	then
		require("mare_lib").fail(
			"stale_session",
			"Cannot switch sessions: the active session changed; agent context may also have changed"
		)
	end
	local agent = st.ctx.agent
	if not rawequal(agent, rev.agent_provider) then
		error("Cannot switch sessions: agent provider changed", 0)
	end
	verify_agent_revision(st, agent, rev)
	return agent
end

-- Commit is non-yielding: agent adoption is a synchronous service contract, then
-- the journal and the shared session table are handed over in the same Lua turn.
local function publish(st, lease, snapshot, rev, messages)
	return call_sync(function()
		if st.disposed or st.switch_lease ~= lease or (st.is_retired and st.is_retired()) then
			error("Cannot switch sessions: session service was disposed", 0)
		end
		require_idle(st)
		if
			st.S.id ~= rev.id
			or st.S.leaf ~= rev.leaf
			or #st.S.list ~= rev.count
			or st.S.file ~= rev.file
			or not table_revision().unchanged(rev.state)
		then
			require("mare_lib").fail(
				"stale_session",
				"Cannot switch sessions: the active session changed; agent context may also have changed"
			)
		end
		local agent = st.ctx.agent
		if not rawequal(agent, rev.agent_provider) then
			error("Cannot switch sessions: agent provider changed", 0)
		end
		local target = snapshot.file
		if not target_matches(lease, snapshot, target) or not source_matches(lease) then
			error("Cannot switch sessions: the file changed while it was being prepared", 0)
		end
		if snapshot.queue ~= nil then
			require("mare_lib").validate_inbox(snapshot.queue)
		end
		verify_agent_revision(st, agent, rev)
		if not rawequal(st.ctx.agent, agent) or not table_revision().unchanged(rev.state) then
			error("Cannot switch sessions: the active session or agent provider changed", 0)
		end
		if not target_matches(lease, snapshot, target) or not source_matches(lease) then
			error("Cannot switch sessions: the file changed while it was being prepared", 0)
		end
		activate_and_adopt(st, lease, snapshot, rev, messages, agent)
		local prev_ended
		if mare.mode ~= "child" and st.S.id and st.S.started and not st.S.ended_for_id and not st.S.close_failed then
			prev_ended = {
				session_id = st.S.id,
				cwd = st.S.cwd or mare.cwd,
				path = st.S.path or st.S.planned,
				reason = lease.end_reason or "switch",
			}
		end
		local had_open_writer = st.S.file ~= nil
		local old_source = lease.defer_source_close or lease.source_file
		local deferred_old = old_source and rawequal(old_source, st.S.file) and st.S.file or nil
		local reused_writer = (not deferred_old) and target ~= nil and rawequal(target, st.S.file)
		if deferred_old then
			-- A source that is also the active writer must retain its lock until the
			-- new state is installed; retire that writer immediately afterwards.
			st.S.file = nil
		elseif reused_writer then
			st.S.file = nil
		end
		local closed, close_err, close_outcome = st.close_file(lease.close_entries)
		if not closed or close_outcome == "failed" then
			st.S.close_failed = true
			poison("session publication failed: " .. tostring(close_err))
			error("session publication failed: " .. tostring(close_err), 0)
		end
		local checked, matches = pcall(function()
			return target_matches(lease, snapshot, target) and source_matches(lease)
		end)
		if not checked or not matches then
			if deferred_old then
				st.S.file = deferred_old
			end
			local err = "session publication failed: source or target changed during old session close"
			poison(err .. (not checked and ": " .. tostring(matches) or ""))
			error(err, 0)
		end
		if st.D and st.D.freeze_baseline_cuts then
			st.D.freeze_baseline_cuts()
		end
		for k in pairs(st.S) do
			st.S[k] = nil
		end
		for k, v in pairs(snapshot) do
			st.S[k] = v
		end
		lease.committed = true
		if st.ctx and st.ctx.kernel and st.ctx.kernel.mark_call_committed then
			st.ctx.kernel:mark_call_committed()
		end
		st.S.started = true
		local confirmed_retired = (not reused_writer) and (not had_open_writer or close_outcome == "retired")
		if deferred_old then
			retire_deferred_old(st, lease, deferred_old)
			confirmed_retired = true
		end
		-- Keep the source lock until the new live state is installed, then discard
		-- its inactive reservation before completion can fail to start.
		if lease.discard_target and lease.target and lease.target ~= target then
			local discarded, discard_err = pcall(st.checked_call, lease.target.discard, lease.target)
			if not discarded then
				poison("session publication failed: target discard failed: " .. tostring(discard_err))
				error("session publication failed: target discard failed: " .. tostring(discard_err), 0)
			end
		end
		lease.ended_snapshot = confirmed_retired and prev_ended or nil
		lease.target = nil
	end)
end

local function finish_switch(st, lease, fn)
	local tx, rx = mare.task.channel()
	local token = {}
	lease.completion_token = token
	local ok, task = pcall(session_spawn, st.ctx, function()
		lease.completion_started = true
		if lease.identity_switch and lease.identity_switch.adopt then
			pcall(lease.identity_switch.adopt, lease.identity_switch, true)
		end
		local sent = false
		if poison_reason() then
			lease.completion_token = nil
			if lease.identity_switch then
				pcall(lease.identity_switch.abort, lease.identity_switch)
				lease.identity_switch = nil
			end
			release_switch(st, lease)
			tx:send(table.pack(false, "session completion was interrupted"))
			return
		end
		lease.completion_task = mare.task.id()
		local finish <close> = setmetatable({}, {
			__close = function()
				lease.completion_task = nil
				lease.completion_token = nil
				if lease.identity_switch then
					pcall(lease.identity_switch.abort, lease.identity_switch)
					lease.identity_switch = nil
				end
				if not sent then
					poison("session completion was interrupted")
					tx:send(table.pack(false, "session completion was interrupted"))
				end
				release_switch(st, lease)
			end,
		})
		if lease.retirement_warning then
			local note = lease.retirement_warning
			lease.retirement_warning = nil
			pcall(st.warn, note)
		end
		local failure
		local function attempt(step, critical)
			local ok_step, err = pcall(step)
			if not ok_step then
				if critical then
					poison("session completion failed: " .. tostring(err))
				end
				failure = failure or err
			end
		end
		if lease.ended_snapshot then
			local ended_snap = lease.ended_snapshot
			lease.ended_snapshot = nil
			attempt(function()
				st.ctx.bus:emit("session/closed", ended_snap)
			end)
		end
		local result = table.pack(pcall(fn, attempt))
		if result[1] and failure then
			result = table.pack(false, "session committed but completion failed: " .. tostring(failure))
		elseif not result[1] then
			poison("session completion failed unexpectedly: " .. tostring(result[2]))
			result[2] = "session committed but completion failed: " .. tostring(result[2])
		end
		if not result[1] then
			-- a log line, not a plugin failure (a logger error is one: problems and kernel/error)
			mare.log("error", result[2])
		end
		lease.completion_task = nil
		lease.completion_token = nil
		if lease.identity_switch then
			pcall(lease.identity_switch.abort, lease.identity_switch)
			lease.identity_switch = nil
		end
		release_switch(st, lease)
		sent = true
		tx:send(result)
	end)
	if not ok then
		lease.completion_token = nil
		if lease.identity_switch then
			pcall(lease.identity_switch.abort, lease.identity_switch)
			lease.identity_switch = nil
		end
		poison("session committed but completion could not start: " .. tostring(task))
		release_switch(st, lease)
		error("session committed but completion could not start: " .. tostring(task), 0)
	end
	if not lease.completion_started and lease.identity_switch and lease.identity_switch.adopt then
		pcall(lease.identity_switch.adopt, lease.identity_switch, false)
	end
	lease.transferred = true
	local result = rx:recv()
	if not result then
		error("session committed but completion was interrupted", 0)
	end
	if not result[1] then
		error(result[2], 0)
	end
	return table.unpack(result, 2, result.n)
end

return function(opts)
	local D = opts.D
	local st = {
		D = D,
		warn = opts.warn,
		checked_call = opts.checked_call,
		close_file = opts.close_file,
		is_retired = opts.is_retired,
		has_active_writes = opts.has_active_writes,
		ctx = D.ctx,
		S = D.S,
		dir = D.dir,
		-- busy can turn false before agent/end listeners finish; track the whole event dispatch.
		turns_in_flight = D.ctx.agent:activity().busy and 1 or 0,
		-- Only the transaction owner may release this lease. The to-be-closed scope
		-- runs on normal exit, errors and cancellation of a Lua async call.
		---@type table?
		switch_lease = nil,
		disposed = false,
	}
	st.ctx:on("turn/started", function()
		st.turns_in_flight = st.turns_in_flight + 1
	end, { prepend = true })
	-- a run is over once every agent/end listener returned: the agent's agent/settled (listener
	-- order is registration order on the bus, no numeric priority)
	st.ctx:on("agent/settled", function()
		st.turns_in_flight = math.max(0, st.turns_in_flight - 1)
	end)
	st.ctx:effect(function()
		return function()
			st.disposed = true
			poison_interrupted_completion(st)
			if st.switch_lease and st.switch_lease.identity_switch then
				pcall(st.switch_lease.identity_switch.abort, st.switch_lease.identity_switch)
				st.switch_lease.identity_switch = nil
			end
			if st.switch_lease and st.switch_lease.stage then
				pcall(st.switch_lease.stage.close, st.switch_lease.stage)
			end
			if st.switch_lease and st.switch_lease.target then
				pcall(
					st.switch_lease.discard_target and st.switch_lease.target.discard or st.switch_lease.target.close,
					st.switch_lease.target
				)
			end
			st.switch_lease = nil
		end
	end, "session switch state")

	return {
		drain_side = function(wait_turn, expected_owner)
			return drain_side(st, wait_turn, expected_owner)
		end,
		drain_subagents = function()
			local lib = require("mare_lib")
			local subagent = st.D.follow.subagent and st.D.follow.subagent()
			local drain = subagent and subagent.drain
			if lib.callable(drain) or (mare.subagent and mare.subagent.drain_session) then
				local ok, err
				if drain then
					ok, err = pcall(drain, subagent)
				else
					ok, err = pcall(mare.subagent.drain_session)
				end
				if not ok then
					lib.fail("busy", "Cannot switch sessions: " .. tostring(err))
				end
			end
			if st.D.run_session_drains then
				local ok_res, err_res = pcall(st.D.run_session_drains)
				if not ok_res then
					lib.fail("busy", "Cannot switch sessions: " .. tostring(err_res))
				end
			end
		end,
		require_idle = function()
			return require_idle(st)
		end,
		turn_active = function()
			return turn_active(st)
		end,
		switching = function()
			return switching(st)
		end,
		begin_switch = function()
			return begin_switch(st)
		end,
		poison_interrupted_completion = function()
			return poison_interrupted_completion(st)
		end,
		release_switch = function(lease)
			return release_switch(st, lease)
		end,
		requests_pending = function(owner)
			return requests_pending(st, owner)
		end,
		new_session_id = new_session_id,
		new_snapshot = function(cwd, id)
			return new_snapshot(st, cwd, id)
		end,
		revision = function()
			return revision(st)
		end,
		verify_revision = function(lease, rev)
			return verify_revision(st, lease, rev)
		end,
		publish = function(lease, snapshot, rev, messages)
			return publish(st, lease, snapshot, rev, messages)
		end,
		finish_switch = function(lease, fn)
			return finish_switch(st, lease, fn)
		end,
		is_transferred = function()
			return is_transferred(st)
		end,
		completion_token = function()
			return completion_token(st)
		end,
		matches_completion = function(token)
			return matches_completion(st, token)
		end,
		commit_identity = function(session_id, force_epoch)
			local lease = st.switch_lease
			local commit_opts = force_epoch and { force_epoch = true } or nil
			if lease and lease.identity_switch then
				local sw <close> = lease.identity_switch
				lease.identity_switch = nil
				return sw:commit(session_id, commit_opts)
			end
			if st.ctx and st.ctx.kernel and st.ctx.kernel.begin_session_switch then
				local sw <close> = st.ctx.kernel:begin_session_switch({ session_id = session_id })
				return sw:commit(session_id, commit_opts)
			end
		end,
	}
end
