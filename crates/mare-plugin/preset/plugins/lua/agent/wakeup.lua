-- agent.wakeup: durable inbox / notification wake-up coordination for the `agent` service.
local poison_reason = mare.task.session_error

return function(outcome)
	local M = {}

	--- Park an input so it remains visible without starting a turn by itself.
	function M.park_input(s, id)
		if id then
			s.parked_inputs = s.parked_inputs or {}
			s.parked_inputs[id] = true
		end
	end

	--- Park inputs restored on startup or retained at cancellation.
	function M.park_existing_inputs(s)
		local idle = {}
		for _, item in ipairs(s.A.steer or {}) do
			if item.input_id then
				idle[item.input_id] = true
			end
		end
		for _, item in ipairs(s.A.follow or {}) do
			if item.input_id then
				idle[item.input_id] = true
			end
		end
		s.parked_inputs = next(idle) and idle or nil
	end

	--- An unsuccessful run must not automatically retry inputs it left behind.
	function M.park_failed_inputs(s, res)
		local failed = res and res.error and res.termination_reason ~= "interrupted"
		if failed or s.A.halt then
			M.park_existing_inputs(s)
		end
	end

	--- True iff the durable inbox holds an item that can start a turn.
	function M.has_wakeable_inbox(s)
		local idle = s.parked_inputs
		for _, queue in ipairs({ s.A.steer, s.A.follow }) do
			for _, item in ipairs(queue or {}) do
				if not (idle and item.input_id and idle[item.input_id]) then
					return true
				end
			end
		end
		return false
	end

	local function run_notes_turn(s)
		local run_session_id = s.A.session_id
		s.A.busy = true
		s.A.run_id = s.A.run_id + 1
		s.A.token = mare.task.token()
		s.A.cancel_reason = nil
		s.A.turn_id = mare.crypto.random_hex(8)
		local turn_id = s.A.turn_id
		s.register_turn_live(s, turn_id)
		s.ctx.bus:emit("turn/started", { turn_id = turn_id, trigger = "notification" })
		local ok, res = pcall(function()
			return s.run(s.take_notes(), {})
		end)
		ok, res = s.complete_run(s, ok, res)
		s.A.busy = false
		if not ok then
			res = outcome.failure(res)
		end
		M.park_failed_inputs(s, res)
		s.publish_end(s, res, turn_id)
		if not s.disposed and s.A.session_id == run_session_id and not (res and res.not_sent) then
			M.wake_pending(s, run_session_id)
		end
	end

	local function run_pending(s, expected_session)
		-- Wait for reload/provider swaps before consuming inbox items or running notes.
		s.ctx.kernel:reload_barrier()
		if s.disposed or poison_reason() then
			return
		end
		if expected_session ~= nil and s.A.session_id ~= expected_session then
			return
		end
		if s.A.busy or s.switching(s) or s.compacting(s) then
			return
		end
		if M.has_wakeable_inbox(s) then
			s.A.wake = false
			local opts = { queued = true }
			local turn_id = s.claim_prompt(s, opts)
			s.finish_prompt(s, nil, opts, turn_id)
			return
		end
		if s.A.wake and #s.A.notes > 0 then
			s.A.wake = false
			run_notes_turn(s)
		end
	end

	--- Wake an idle agent for either live queued inputs (`steer`/`follow`) or pending `wake` notes.
	function M.wake_pending(s, expected_session)
		if s.disposed or poison_reason() or s.A.busy or s.switching(s) or s.compacting(s) then
			return
		end
		if expected_session ~= nil and s.A.session_id ~= expected_session then
			return
		end
		if not M.has_wakeable_inbox(s) and not (s.A.wake and #s.A.notes > 0) then
			return
		end
		s.ctx:spawn(function()
			run_pending(s, expected_session)
		end)
	end

	return M
end
