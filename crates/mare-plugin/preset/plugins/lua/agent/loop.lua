-- agent.loop: the agent loop (run: requests, retries, tool dispatch, steering); part of the `agent` plugin (plugins/agent.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg, A = D.ctx, D.cfg, D.A
	local preflight = require("agent.loop_preflight")(D)
	local request = require("agent.loop_request")(D)
	local tools_dispatch = require("agent.loop_tools")(D)
	local append = D.persist.append
	local build_user = D.inputs.build_user

	local function acquire_turn_lease(S, first_lease, lease_state)
		if S.token:is_cancelled() then
			S.interrupt()
			return nil
		end
		S.turns = S.turns + 1
		if S.turns > cfg.max_turns then
			S.run_error = "max turns exceeded"
			S.termination_reason = "turn_limit"
			S.error_kind = "max_turns"
			return nil
		end
		local lease, lease_error
		if not lease_state.used_first and first_lease then
			lease_state.used_first = true
			lease = first_lease
		elseif D.ctx and D.ctx.kernel then
			lease, lease_error = D.ctx.kernel:request_lease(S.token)
		else
			lease, lease_error = nil, "kernel.request_lease unavailable"
		end
		if not lease then
			if S.token:is_cancelled() then
				S.interrupt()
			else
				S.run_error = lease_error or "request readiness was not approved"
			end
			return nil
		end
		return lease
	end

	local function consume_or_fail(S, kind, skip_parked)
		local items, input_error, deny_abort = D.inputs.consume(kind, S.token, nil, skip_parked)
		if items then
			return items
		end
		S.run_error = input_error
		if deny_abort then
			S.termination_reason = "error"
			S.error_kind = "aborted"
			A.halt = input_error or "input denied"
			S.token:cancel()
		elseif S.token:is_cancelled() then
			S.interrupt()
		else
			S.termination_reason = "error"
		end
		return nil
	end

	local function handle_turn_end(S, msg, calls, terminate, opts)
		local skip_parked = opts and opts.skip_parked
		if A.halt then
			S.run_error = A.halt
			S.termination_reason = "error"
			return false
		end
		if S.token:is_cancelled() then
			S.interrupt()
			return false
		end
		local cont_msg
		if
			(#calls == 0 or terminate)
			and not D.inputs.has_pending("steer", skip_parked)
			and #A.notes == 0
			and not D.inputs.has_pending("follow_up", skip_parked)
		then
			-- agent/turn-stopping (bail): a listener may keep the loop going (goal / Ralph loop)
			-- by returning a message (string or {message=}) that is appended after step-ready admits.
			-- `turn_id` and `cancel` (the run's token) let a listener tie its state to this run and stop
			-- when it is aborted (a Stop hook refuses to decide without them)
			local cont = ctx.bus:bail(
				"agent/turn-stopping",
				{ message = msg, turn = S.turns, turn_id = A.turn_id, cancel = S.token }
			)
			-- aborted while a listener decided (a Stop hook that stops the run, or the user):
			-- interrupted, as an abort anywhere else in the turn (turn_aborted marker, not an error)
			if S.token:is_cancelled() then
				S.interrupt()
				return false
			end
			local cont_source
			if type(cont) == "table" then
				if type(cont.source) == "string" and cont.source ~= "" then
					cont_source = cont.source
				end
				cont = cont.message
			end
			if type(cont) ~= "string" or cont == "" then
				if D.inputs.has_pending("steer", skip_parked) or #A.notes > 0 then
					return true, { calls = calls, terminate = terminate }
				end
				return false
			end
			cont_msg = cont
			return true, { calls = calls, terminate = terminate, cont = cont_msg, cont_source = cont_source }
		end
		return true, { calls = calls, terminate = terminate, cont = cont_msg }
	end

	--- Run the loop until the model stops and the follow-up queue is empty.
	local function run(first_user, opts)
		local S = preflight.new_state(opts)
		if not S.run_error then
			preflight.check_readiness(S)
		end
		local first_lease <close> = preflight.acquire_first_lease(S)
		if S.run_error then
			preflight.enqueue_initial_inputs(S, first_user, opts)
		end

		local tool_cache = { defs = nil, model = nil, gen = nil }
		local lease_state = { used_first = false, enqueued_initial = false }
		local pending_turn_end = nil
		while not S.run_error do
			-- A request owns the reload gate from assembly through streaming. Closing the Lua
			-- scope (including cancellation/error) releases it; tool execution happens later.
			local lease = acquire_turn_lease(S, first_lease, lease_state)
			if not lease then
				break
			end
			local request_lease <close> = lease
			-- Admit the model step (agent/step-ready) before consuming initial or queued inputs,
			-- then stage state participants so they see the admitted input while denials leave
			-- conversation history and queued inputs untouched.
			local function on_admit()
				if not lease_state.enqueued_initial then
					lease_state.enqueued_initial = true
					preflight.enqueue_initial_inputs(S, first_user, opts)
					if S.run_error then
						return false, S.run_error
					end
				elseif pending_turn_end then
					local pending = pending_turn_end
					pending_turn_end = nil
					local steered = consume_or_fail(S, "steer", opts and opts.skip_parked)
					if not steered then
						return false, S.run_error
					end
					local noted = #A.notes > 0
					if noted then
						preflight.deliver_notes(S, D.inputs.take_notes())
					end
					if (#pending.calls == 0 or pending.terminate) and #steered == 0 and not noted then
						local follow = consume_or_fail(S, "follow_up", opts and opts.skip_parked)
						if not follow then
							return false, S.run_error
						end
						if #follow == 0 and pending.cont then
							append(
								build_user(pending.cont, nil, nil, pending.cont_source, nil, pending.cont_source ~= nil)
							)
						end
					end
				end
				return true
			end
			local reservation, reservation_error, reservation_kind =
				D.steps.begin(S.turns, S.token, A.turn_id, on_admit)
			if not reservation then
				if not S.run_error then
					request.fail_step(S, reservation_error, reservation_kind)
				end
				break
			end
			local step_reservation <close> = setmetatable({}, {
				__close = function()
					D.steps.release(reservation)
				end,
			})
			-- Establish the step boundary before context and request hooks can yield. Subscribers
			-- such as MCP must not replace tools while this request is being assembled.
			S.requested = true
			ctx.bus:emit("step/started", {
				turn_id = A.turn_id,
				step_id = reservation.id,
				turn = S.turns,
				model = ctx.llm:current(),
				attempt = 0,
			})
			local function step_error(msg_text, code, data)
				if msg_text == nil then
					return nil
				end
				if
					type(msg_text) == "table"
					and type(msg_text.code) == "string"
					and type(msg_text.message) == "string"
				then
					return msg_text
				end
				local out = {
					code = (type(code) == "string" and code ~= "" and code) or "error",
					message = tostring(msg_text),
				}
				if data ~= nil then
					out.data = data
				end
				return out
			end
			local step_closed = false
			local active_reset_id = nil
			local function emit_step_completed(payload)
				step_closed = true
				ctx.bus:emit("step/completed", payload)
			end
			local step_guard <close> = setmetatable({}, {
				__close = function(_, err)
					if active_reset_id then
						pcall(active_reset_id, S.token:is_cancelled() and "cancelled" or "failed")
						active_reset_id = nil
					end
					if not step_closed then
						step_closed = true
						local cancelled = S.termination_reason == "interrupted" or S.token:is_cancelled()
						pcall(function()
							ctx.bus:emit("step/completed", {
								turn_id = A.turn_id,
								step_id = reservation.id,
								status = cancelled and "cancelled" or "failed",
								error = step_error(
									err or S.run_error or "step aborted unexpectedly",
									S.error_kind or (cancelled and "aborted" or "error")
								),
								will_retry = false,
							})
						end)
					end
				end,
			})
			local req, decl_holder = request.build_request(S, opts, tool_cache, reservation)
			if not req then
				emit_step_completed({
					turn_id = A.turn_id,
					step_id = reservation.id,
					status = S.termination_reason == "interrupted" and "cancelled" or "failed",
					error = step_error(S.run_error, S.error_kind or S.termination_reason),
					error_kind = S.error_kind,
					will_retry = false,
				})
				break
			end
			local sink, flush, is_message_started, get_id, reset_id =
				request.create_stream_sink(req.model, reservation.id)
			active_reset_id = reset_id
			local msg, rid = request.execute_request(S, req, request_lease, sink, flush, reset_id)
			local started = is_message_started()
			if request.try_recover_response(S, msg, started, get_id(), reset_id) == "retry" then
				emit_step_completed({
					turn_id = A.turn_id,
					step_id = reservation.id,
					status = "failed",
					stop_reason = msg.stop_reason,
					error = step_error(msg.error, msg.error_code or msg.error_kind, msg.error_data),
					error_kind = msg.error_kind,
					will_retry = true,
					reset = started,
				})
				if started and reset_id then
					reset_id("failed", msg)
				end
				active_reset_id = nil
				goto continue
			end
			local action, truncated =
				request.handle_response_outcome(S, msg, rid, is_message_started(), get_id(), reset_id)
			active_reset_id = nil
			if action == "break" then
				emit_step_completed({
					turn_id = A.turn_id,
					step_id = reservation.id,
					status = S.termination_reason == "interrupted" and "cancelled" or "failed",
					stop_reason = msg.stop_reason,
					usage = msg.usage,
					error = step_error(S.run_error, S.error_kind or msg.error_code or msg.error_kind, msg.error_data),
					error_kind = S.error_kind,
					will_retry = false,
				})
				break
			elseif action == "continue" then
				emit_step_completed({
					turn_id = A.turn_id,
					step_id = reservation.id,
					status = "completed",
					stop_reason = msg.stop_reason,
					usage = msg.usage,
					will_retry = false,
				})
				goto continue
			end
			local calls, results, terminate =
				tools_dispatch.dispatch_calls(msg, truncated, S.token, decl_holder, reservation.id)
			emit_step_completed({
				turn_id = A.turn_id,
				step_id = reservation.id,
				status = "completed",
				stop_reason = msg.stop_reason,
				usage = msg.usage,
				will_retry = false,
			})
			local cont, pending = handle_turn_end(S, msg, calls, terminate, opts)
			if not cont then
				break
			end
			pending_turn_end = pending
			::continue::
		end

		return preflight.finalize_run(S)
	end

	return { run = run }
end
