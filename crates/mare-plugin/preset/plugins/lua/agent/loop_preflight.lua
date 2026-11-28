-- agent.loop_preflight: run initialization, readiness checks, initial input consumption, and finalization.
return function(D)
	local ctx, cfg, A = D.ctx, D.cfg, D.A
	-- (on mare.Ctx `policy` is still typed as the transition layer's method)
	local policy = ctx.policy --[[@as mare.PolicyService]]
	local orphans, append = D.persist.orphans, D.persist.append
	local text_msg, drain = D.inputs.text_msg, D.inputs.drain

	local function new_state(opts)
		local S = {
			usage = {},
			last_text = "",
			final_text = "",
			run_error = nil,
			structured = nil,
			termination_reason = "completed",
			error_kind = nil,
			turns = 0,
			requested = false,
			overflow_rounds = 0,
			image_rounds = 0,
			token = A.token,
			compiled_output_schema = nil,
			input_ready_voters_seen = (opts and opts.initial_voters_seen == true)
				or policy:has_voters("agent/input-ready"),
		}
		function S.interrupt()
			S.run_error = "aborted"
			S.termination_reason = "interrupted"
			S.error_kind = "aborted"
		end
		A.run_msgs = mare.json.array()
		A.halt = nil
		-- a previous run that died mid-tool (crash, kill, dispose): record what is known
		orphans().settle()
		if opts and opts.output_schema ~= nil then
			local ok_schema, compiled = pcall(mare.schema.compile_output, opts.output_schema)
			if not ok_schema then
				S.run_error = "invalid output_schema: " .. tostring(compiled)
				S.error_kind = "invalid"
			else
				S.compiled_output_schema = compiled
			end
		end
		return S
	end

	local function check_readiness(S)
		if policy:has_voters("agent/input-ready") then
			S.input_ready_voters_seen = true
		end
		local token = S.token
		-- freshness before the first request: listeners refresh what the
		-- request depends on (instruction files, skills; MCP servers marked `required = true`
		-- are waited for without a cap, `direct` ones are not waited for). Later updates wait
		-- for the next prompt.
		local prepared, prepare_error = pcall(function()
			local args = { model = ctx.llm:current(), cancel = token }
			-- llm's ordered readiness steps first, then the public event for everyone else
			ctx.llm:prepare_request(args)
			ctx.bus:emit_checked("request/prepare", args)
		end)
		-- Both preparation failures and readiness denials stop before request/build,
		-- persistence or provider I/O. The readiness vote is repeated for every prompt.
		if token:is_cancelled() then
			S.interrupt()
		elseif not prepared then
			S.run_error = tostring(prepare_error)
			S.termination_reason = "error"
		else
			-- Own the entire admission callback scope without placing an opaque token in
			-- the public policy payload. Cancellation closes nested provider preparation too.
			local _, readiness = mare.task.select(function()
				return policy:decide("request/ready", { { model = ctx.llm:current() } })
			end, function()
				token:wait()
			end)
			if token:is_cancelled() then
				S.interrupt()
			elseif not readiness or readiness.decision ~= "allow" then
				S.run_error = (readiness and readiness.reason) or "request readiness was not approved"
				S.termination_reason = "error"
			end
		end
	end

	-- Acquire the first request lease before consuming queued inputs or appending the first
	-- user message: a lease failure (reload/poison/concurrent request) refuses the prompt
	-- with `not_sent = true`, so nothing may be consumed or persisted yet.
	local function acquire_first_lease(S)
		if S.run_error then
			return nil
		end
		if S.token:is_cancelled() then
			S.interrupt()
			return nil
		end
		if 1 > cfg.max_turns then
			S.run_error = "max turns exceeded"
			S.termination_reason = "turn_limit"
			S.error_kind = "max_turns"
			return nil
		end
		local lease, lease_error
		if D.ctx and D.ctx.kernel then
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

	local append_custom_checked

	--- Deliver pending notifications (a `take_notes` message) into this turn. On failure they
	--- stay queued for a later request.
	local function deliver_notes(S, msg)
		local ok, err = pcall(D.inputs.deliver_notes, msg, S.token)
		if not ok then
			mare.log("warn", "notifications were not delivered: " .. tostring(err))
		end
	end

	local function enqueue_initial_inputs(S, first_user, opts)
		-- a refused prompt (preparation or readiness: a blocking plugin failure, an init.lua
		-- error, a hook's veto) is not part of the conversation: the frontend gives its text
		-- back to the input box, so sending it again
		-- doesn't repeat it. An interrupt keeps it, as does a turn of notifications (no one
		-- would send those again); pending notifications wait for the next prompt.
		local voters_active = (S and S.input_ready_voters_seen == true) or D.inputs.has_input_ready_voters()
		local refused = S.run_error and S.termination_reason ~= "interrupted"
		if first_user then
			if D.inputs.is_trusted_notification(first_user) then
				if not refused then
					deliver_notes(S, first_user)
				end
			elseif not S.run_error then
				first_user.input_id = first_user.input_id or opts.input_id or mare.crypto.random_hex(16)
				first_user.content = D.inputs.project_content(first_user.content)
				if opts and opts.input_context ~= nil then
					first_user.input_context =
						mare.projection.project(require("mare_lib").copy_deep(opts.input_context))
				end
				first_user.turn_id = A.turn_id
				local attempt_id, deny_reason, deny_abort, deny_kind, admit_had_voters = D.inputs.admit_input({
					input_id = first_user.input_id,
					revision = 1,
					turn_id = A.turn_id,
					cancel = S.token,
					content = first_user.content,
					input_context = opts.input_context,
					source = first_user.source,
					delivery = "prompt",
				})
				if admit_had_voters then
					voters_active = true
				end
				if not attempt_id then
					S.run_error = deny_reason or "input denied"
					if deny_abort then
						S.termination_reason = "error"
						S.error_kind = "aborted"
						A.halt = S.run_error
						S.token:cancel()
					elseif deny_kind == "cancelled" or S.token:is_cancelled() then
						S.interrupt()
						if not voters_active then
							append(first_user)
						else
							S.unadmitted = true
						end
					else
						S.termination_reason = "error"
					end
					return
				end
				first_user.input_attempt_id = attempt_id
				append(first_user)
			elseif S.termination_reason == "interrupted" then
				if not voters_active then
					if opts and opts.input_context ~= nil then
						first_user.input_context =
							mare.projection.project(require("mare_lib").copy_deep(opts.input_context))
					end
					first_user.turn_id = A.turn_id
					append(first_user)
				else
					S.unadmitted = true
				end
			end
		end
		if not S.run_error and opts.queued then
			local messages, input_error, deny_abort = D.inputs.consume("all", S.token, opts.input_id)
			if not messages then
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
			elseif #messages == 0 and #A.notes == 0 and #A.next_turn == 0 then
				S.run_error = "aborted"
				S.termination_reason = "interrupted"
			end
		end
		refused = S.run_error and S.termination_reason ~= "interrupted"
		-- notifications that waited for this prompt go in before the first request
		if not refused and #A.notes > 0 then
			deliver_notes(S, D.inputs.take_notes())
		end
		if not refused and #A.next_turn > 0 then
			for _, m in ipairs(drain(A.next_turn)) do
				append_custom_checked(S, m)
			end
		end
	end

	function append_custom_checked(S, m)
		if type(m) == "table" and m.role == "user" then
			if S and S.token and S.token:is_cancelled() then
				return
			end
			m.input_id = m.input_id or mare.crypto.random_hex(16)
			m.content = D.inputs.project_content(m.content)
			local attempt_id = D.inputs.admit_input({
				input_id = m.input_id,
				revision = 1,
				turn_id = A.turn_id,
				cancel = (S and S.token) or A.token,
				content = m.content,
				source = m.source,
				delivery = "follow_up",
			})
			if not attempt_id then
				return
			end
			m.input_attempt_id = attempt_id
		end
		append(m)
	end

	local CANCELLATION_CAUSES = {
		user = true,
		parent = true,
		hook = true,
		disposed = true,
	}

	local function cancellation_cause_of(reason)
		if type(reason) ~= "table" then
			return "user"
		end
		local root = reason
		while type(root.cause) == "table" do
			root = root.cause
		end
		if root.source == "host" or root.source == "session_switch" then
			return "disposed"
		end
		if type(reason.source) == "string" and CANCELLATION_CAUSES[reason.source] then
			return reason.source
		end
		return "user"
	end

	local function finalize_run(S)
		if S.termination_reason == "interrupted" then
			-- Settle any unexecuted tool calls on the last assistant turn before appending
			-- the turn-aborted marker so no orphan tool_call sits behind `<turn_aborted>`.
			orphans().settle()
			-- A model-visible marker, so the next turn knows the previous one was cut off
			-- (calls that didn't finish already carry "Aborted" results)
			local reason = A.cancel_reason or { source = "user" }
			local root = reason
			while type(root.cause) == "table" do
				root = root.cause
			end
			local who = root.source == "host" and "The host shut down during"
				or root.source == "session_switch" and "The parent session was replaced during"
				or reason.source == "parent" and "The parent cancelled"
				or "The user interrupted"
			local m = text_msg(
				"user",
				"<turn_aborted>"
					.. who
					.. " the previous turn. Anything not reported as done above did not complete.</turn_aborted>"
			)
			m.notification = true
			m.turn_aborted = true
			m.cancel_reason = reason
			append(m)
		end
		if #A.pending_custom > 0 then
			for _, m in ipairs(drain(A.pending_custom)) do
				append_custom_checked(S, m)
			end
		end
		if not S.run_error and S.compiled_output_schema then
			local ok_out, parsed = pcall(S.compiled_output_schema.validate_text, S.compiled_output_schema, S.final_text)
			if not ok_out then
				S.run_error = "output schema validation failed: " .. tostring(parsed)
				S.error_kind = "invalid_output"
			else
				S.structured = parsed
			end
		end
		local save_warning
		if A.persist_error then
			local qpaths = A.quarantine_paths or A.quarantine_path
			local formatted = require("mare_lib").format_save_error(A.persist_error, qpaths, A.quarantine_error)
			if not S.run_error then
				S.run_error = formatted
				S.error_kind = "save_failed"
			elseif A.halt and not S.error_kind then
				S.run_error = require("mare_lib").format_save_error(S.run_error, qpaths, A.quarantine_error)
				S.error_kind = "save_failed"
			else
				save_warning = formatted
			end
		end
		local new_msgs = A.run_msgs or mare.json.array()
		A.run_msgs = nil
		if S.run_error and S.termination_reason == "completed" then
			S.termination_reason = "error"
		end
		-- `error_kind` lets frontends map the outcome (headless exit code 4 for the turn limit)
		local error_kind = S.error_kind or (S.termination_reason == "interrupted" and "aborted") or nil
		return {
			text = S.last_text,
			structured = S.structured,
			messages = new_msgs,
			usage = S.usage,
			error = S.run_error,
			error_kind = error_kind,
			save_warning = save_warning,
			termination_reason = S.termination_reason,
			cancellation_cause = S.termination_reason == "interrupted" and cancellation_cause_of(A.cancel_reason)
				or nil,
			-- stopped before any request (preparation or readiness refused, or cancelled before admission):
			-- the frontend preserves recoverable editor text separately from `<turn_aborted>`.
			not_sent = ((S.run_error and S.termination_reason ~= "interrupted" and not S.requested) or S.unadmitted)
				or nil,
		}
	end

	return {
		new_state = new_state,
		check_readiness = check_readiness,
		acquire_first_lease = acquire_first_lease,
		enqueue_initial_inputs = enqueue_initial_inputs,
		deliver_notes = deliver_notes,
		finalize_run = finalize_run,
	}
end
