-- Optional owner-scoped retained VM memory. Capabilities are rebound on every run.
return function(ctx, cfg, svc)
	local entries = {}
	local lost = {}
	local mode = cfg.execution_mode or "per-call"
	assert(mode == "per-call" or mode == "persistent-in-memory" or mode == "vm-script", "invalid script execution_mode")
	assert(not cfg.script_persistence or mode == "vm-script", "script_persistence requires vm-script")
	local retained = mode == "persistent-in-memory" or (mode == "vm-script" and cfg.script_persistence)
	local function acquire(owner, cancel, deadline)
		if not retained then
			return nil, function() end
		end
		owner = owner or "session"
		assert(type(owner) == "string" and #owner > 0 and #owner <= 256, "invalid script state owner")
		local context = mare.exec.current() or {}
		local session = svc.session
		local authority = mare.crypto.sha256(mare.json.encode_strict({
			sandbox = context.sandbox,
			cwd = context.cwd,
			unconfined = context.unconfined,
			session = session and session:info().session_id or nil,
		}))
		local entry = entries[owner]
		if entry then
			assert(entry.authority == authority, "release the retained environment before changing execution authority")
		end
		if not entry then
			entry = {
				vm = mare.sandbox.new({
					persistent = true,
					timeout_ms = cfg.timeout_ms,
					memory_bytes = cfg.memory_bytes,
					max_bytes = cfg.max_bytes,
					max_message_bytes = cfg.max_message_bytes ~= 0 and cfg.max_message_bytes or cfg.max_bytes,
					max_output_bytes = cfg.max_output_bytes ~= 0 and cfg.max_output_bytes or cfg.max_bytes,
					max_pending_calls = cfg.max_pending_calls,
					stop_grace_ms = cfg.stop_grace_ms,
					grace_ms = cfg.grace_ms,
					max_instructions = cfg.max_instructions,
				}),
				busy = false,
				authority = authority,
			}
			entries[owner] = entry
		end
		assert(not entry.quarantined, "retained environment stop is unconfirmed; release it before reuse")
		assert(not entry.busy, "retained owner already has an active execution")
		assert(entries[owner] == entry, "script owner environment changed during admission")
		entry.busy = true
		local state_lost = lost[owner]
		lost[owner] = nil
		return entry.vm, function()
			entry.busy = false
		end, state_lost
	end
	local function invalidate()
		for key, entry in pairs(entries) do
			lost[key] = true
			entry.vm:stop()
		end
		entries = {}
	end
	local function release(owner)
		local entry = entries[owner]
		if not entry then
			return true
		end
		entry.quarantined = true
		if entry.busy or not entry.vm:close() then
			return false
		end
		entries[owner] = nil
		lost[owner] = true
		return true
	end
	ctx:on("session/changed", invalidate)
	ctx:on("sandbox/changed", invalidate)
	ctx:on("permission/changed", invalidate)
	ctx:effect(function()
		return invalidate
	end, "cleanup")
	return {
		retained = retained,
		acquire = acquire,
		release = release,
		discard = function(owner)
			local entry = entries[owner]
			if entry then
				entry.vm:stop()
				entries[owner] = nil
			end
		end,
	}
end
