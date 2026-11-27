-- Execution-state acknowledgements share the session's serialized writer.
return function(D)
	local S = D.S
	local lib = require("mare_lib")

	local function write(ev)
		if type(ev) ~= "table" then
			lib.fail("invalid_arguments", "expected a tool state object")
		end
		local owner = D.store.check_owner(ev, true)
		if type(ev.tool_call_id) ~= "string" or ev.tool_call_id == "" or type(ev.name) ~= "string" or ev.name == "" then
			lib.fail("invalid_arguments", "tool_call_id and name must be nonempty strings")
		end
		if ev.state ~= "prepared" and ev.state ~= "started" and ev.state ~= "completed" then
			lib.fail("invalid_arguments", "unknown execution phase")
		end
		if
			ev.side_effect ~= nil
			and ev.side_effect ~= "read"
			and ev.side_effect ~= "write"
			and ev.side_effect ~= "external"
		then
			lib.fail("invalid_arguments", "invalid side_effect")
		end
		if ev.idempotent ~= nil and type(ev.idempotent) ~= "boolean" then
			lib.fail("invalid_arguments", "idempotent must be boolean")
		end
		if ev.durable ~= nil and type(ev.durable) ~= "boolean" then
			lib.fail("invalid_arguments", "durable must be boolean")
		end
		if ev.cancel ~= nil and not mare.task.is_token(ev.cancel) then
			lib.fail("invalid_arguments", "cancel must be a token")
		end
		D.store.check_new_id(ev.entry_id, true)
		local ext_id = ev.entry_id
		local cancel, durable = ev.cancel, ev.durable == true
		local skip_q = ev.skip_quarantine == true
		if S.readonly then
			local side_effect = ev.side_effect or "external"
			if side_effect ~= "read" and ev.state ~= "prepared" then
				lib.fail("persist_failed", "tool state could not be saved: durable execution state is unavailable")
			end
			local id = ext_id or D.store.new_id()
			S.tool_states[ev.tool_call_id] = {
				state = ev.state,
				side_effect = side_effect,
				idempotent = ev.idempotent == true,
			}
			return { status = "read_only", entry_id = id }
		end
		if S.unstable or (durable and D.disabled) then
			lib.fail("persist_failed", "tool state could not be saved: durable execution state is unavailable")
		end
		local id, qpath, qerr
		local ok, err, qp_outer, qe_outer, err_code = D.store.run_host(function(valid)
			if cancel and cancel:is_cancelled() then
				return nil, "aborted"
			end
			if valid and not valid() then
				return D.store.stale_result()
			end
			id = ext_id or D.store.new_id()
			local entry = {
				type = "tool_state",
				entry_id = id,
				tool_call_id = ev.tool_call_id,
				name = ev.name,
				state = ev.state,
				side_effect = ev.side_effect or "external",
				idempotent = ev.idempotent == true,
				created_at_ms = lib.to_epoch_ms(mare.task.now()),
			}
			local written, reason, qp, qe, acode = D.store.add(entry, not D.disabled, durable, valid, function()
				S.tool_states[entry.tool_call_id] = {
					state = entry.state,
					side_effect = entry.side_effect,
					idempotent = entry.idempotent,
				}
			end, skip_q and { skip_quarantine = true } or nil)
			qpath, qerr = qp, qe
			if not written then
				return nil, reason, qp, qe, acode
			end
			return true
		end, owner)
		if not ok then
			D.store.fail_spi(err, qpath or qp_outer, qerr or qe_outer, (qpath or qp_outer) ~= nil, false, err_code)
		end
		return id
	end

	function D.session.record_tool_state(_, _, ev)
		return write(ev)
	end
end
