-- agent.persist: the persistence barrier (records, fault queue, append) and the lazy orphans hook;
-- part of the `agent` plugin (plugins/agent.lua), loaded at the start of its apply.
return function(D)
	local ctx, A = D.ctx, D.A
	local function owner()
		local session = D.follow.session()
		local info = session and session:info() or nil
		return {
			session_id = info and info.session_id or A.session_id,
			session_epoch = info and info.session_epoch or nil,
		}
	end
	local function same_owner(a, b)
		return a.session_id == b.session_id and a.session_epoch == b.session_epoch
	end
	local function hold_write()
		A.persist_inflight = A.persist_inflight + 1
		return setmetatable({}, {
			__close = function()
				A.persist_inflight = A.persist_inflight - 1
			end,
		})
	end

	local function record_quarantine_path(path)
		if type(path) ~= "string" or path == "" then
			return
		end
		if not A.quarantine_path then
			A.quarantine_path = path
		end
		if not A.quarantine_paths then
			A.quarantine_paths = {}
		end
		for _, existing in ipairs(A.quarantine_paths) do
			if existing == path then
				return
			end
		end
		A.quarantine_paths[#A.quarantine_paths + 1] = path
	end

	local function quarantine_agent_record(rec, record_owner, reason)
		local state_dir = mare.paths and mare.paths.state
		if not state_dir or state_dir == "" then
			return nil, "missing state directory"
		end
		local lib = require("mare_lib")
		local sid = record_owner and record_owner.session_id or A.session_id
		local epoch = record_owner and record_owner.session_epoch or nil
		if not A.quarantine or A.quarantine.session_id ~= sid or A.quarantine.session_epoch ~= epoch then
			A.quarantine = {
				session_id = sid,
				session_epoch = epoch,
				path = A.quarantine_path or lib.quarantine_path(state_dir, sid),
			}
		elseif A.quarantine_path and A.quarantine.path ~= A.quarantine_path then
			A.quarantine.path = A.quarantine_path
		end
		local entry
		if rec.kind == "message" then
			entry = { type = "message", message = rec.msg, timestamp = mare.task.now() }
		else
			entry = {
				type = "custom",
				kind = "agent/" .. tostring(rec.kind),
				data = rec.payload,
				timestamp = mare.task.now(),
			}
		end
		local qpath, qerr = lib.quarantine_records(A.quarantine.path, { entry }, reason, state_dir)
		if qpath then
			mare.log("warn", ("agent persistence failed; quarantined record to %s: %s"):format(qpath, tostring(reason)))
		else
			mare.log(
				"error",
				("agent persistence failed and quarantine failed (%s): %s"):format(tostring(qerr), tostring(reason))
			)
		end
		return qpath, qerr
	end

	--- write one record through the `session` persistence SPI (`persist_message`,
	--- `record_tool_state`, `record_request*`). `durable` asks for an fsync (fault recovery).
	--- A persistent session requires a live session provider. Only --no-session and
	--- child sessions (without a transcript writer) may proceed without one.
	local function write_record(rec, durable, record_owner, already_quarantined, already_quarantined_initial_model)
		local r, ok, err_val
		durable = durable or rec.durable
		local skip_q = already_quarantined or nil
		local skip_init_q = already_quarantined_initial_model or nil
		local session = D.follow.session and D.follow.session()
		if not session then
			ok = true
			r = nil
		elseif rec.kind == "run-end" then
			local p = table.copy(rec.payload)
			p.durable = durable or nil
			p.session_id = record_owner and record_owner.session_id or nil
			p.session_epoch = record_owner and record_owner.session_epoch or nil
			p.skip_quarantine = skip_q
			ok, err_val = pcall(session.record_run_end, session, p)
			r = err_val
		elseif rec.kind == "message" then
			ok, err_val = pcall(session.persist_message, session, {
				entry_id = rec.id,
				message = rec.msg,
				durable = durable or nil,
				session_id = record_owner and record_owner.session_id or nil,
				session_epoch = record_owner and record_owner.session_epoch or nil,
				skip_quarantine = skip_q,
				skip_initial_model_quarantine = skip_init_q,
			})
			r = err_val
		elseif rec.kind == "request" or rec.kind == "request-end" or rec.kind == "request-aborted" then
			local p = table.copy(rec.payload)
			p.durable = durable or nil
			p.session_id = record_owner and record_owner.session_id or nil
			p.session_epoch = record_owner and record_owner.session_epoch or nil
			p.skip_quarantine = skip_q
			local method_name = rec.kind == "request" and "record_request"
				or (rec.kind == "request-end" and "record_request_end" or "record_request_aborted")
			ok, err_val = pcall(session[method_name], session, p)
			r = err_val
		else
			local p = table.copy(rec.payload)
			p.durable = durable or nil
			p.session_id = record_owner and record_owner.session_id or nil
			p.session_epoch = record_owner and record_owner.session_epoch or nil
			p.skip_quarantine = skip_q
			ok, err_val = pcall(session.record_tool_state, session, p)
			r = err_val
		end
		if not ok then
			local err_table = type(err_val) == "table" and err_val or nil
			local data = err_table and type(err_table.data) == "table" and err_table.data or {}
			local err = err_table and err_table.message or tostring(err_val)
			if data.quarantine_path then
				record_quarantine_path(data.quarantine_path)
			end
			if data.quarantine_error and not A.quarantine_error then
				A.quarantine_error = data.quarantine_error
			end
			local q_init = already_quarantined_initial_model == true or data.quarantined_initial_model == true
			local handled = already_quarantined == true or data.quarantined_current == true
			if not handled then
				local qpath, qerr = quarantine_agent_record(rec, record_owner, err)
				if qpath then
					record_quarantine_path(qpath)
					handled = true
				end
				if qerr then
					A.quarantine_error = qerr
				end
			end
			return nil, err, handled, q_init
		end
		if type(r) == "table" and r.error then
			local err = tostring(r.error)
			if r.quarantine_path then
				record_quarantine_path(r.quarantine_path)
			end
			if r.quarantine_error and not A.quarantine_error then
				A.quarantine_error = r.quarantine_error
			end
			local q_init = already_quarantined_initial_model == true or r.quarantined_initial_model == true
			local handled = already_quarantined == true or r.quarantined_current == true
			if not handled then
				local qpath, qerr = quarantine_agent_record(rec, record_owner, err)
				if qpath then
					record_quarantine_path(qpath)
					handled = true
				end
				if qerr then
					A.quarantine_error = qerr
				end
			end
			return nil, err, handled, q_init
		end
		local acked = r == true or (type(r) == "string" and r ~= "") or (type(r) == "table" and r.status == "read_only")
		if not acked and not ((mare.args and mare.args.no_session) or mare.mode == "child") then
			local err = "session persistence was not acknowledged; enable a session provider or use --no-session"
			local q_init = already_quarantined_initial_model == true
			local handled = already_quarantined == true
			if not handled then
				local qpath, qerr = quarantine_agent_record(rec, record_owner, err)
				if qpath then
					record_quarantine_path(qpath)
					handled = true
				end
				if qerr then
					A.quarantine_error = qerr
				end
			end
			return nil, err, handled, q_init
		end
		return true, nil, nil, nil, type(r) == "string" and r ~= "" and r or nil
	end

	--- Re-write the records that failed, in order, durably. The fault is cleared only when all
	--- of them made it.
	local function flush_pending()
		local held <close> = hold_write()
		while #A.pending > 0 do
			local pending, current = A.pending[1], owner()
			if not same_owner(pending.owner, current) then
				A.persist_error = "pending session records belong to a different session generation"
				return false
			end
			local ok, err, q_now, q_init_now = write_record(
				pending.record,
				true,
				pending.owner,
				pending.quarantined,
				pending.initial_model_quarantined
			)
			if not ok then
				if q_now then
					pending.quarantined = true
				end
				if q_init_now then
					pending.initial_model_quarantined = true
				end
				A.persist_error = err
				return false
			end
			table.remove(A.pending, 1)
		end
		if A.persist_error then
			A.persist_error = nil
			A.quarantine_path = nil
			A.quarantine_paths = nil
			A.quarantine_error = nil
			A.quarantine = nil
			ctx.bus:emit("host/notice", { level = "info", text = "session writes recovered" })
		end
		return true
	end

	--- persistence barrier: every record goes through here, in order. During a
	--- fault new records queue behind the missing ones (never written out of order).
	local function persist(rec, record_owner)
		local held <close> = hold_write()
		record_owner = record_owner or owner()
		if #A.pending > 0 then
			flush_pending()
		end
		if not same_owner(record_owner, owner()) then
			A.persist_error = "pending session records belong to a different session generation"
			local qpath, qerr = quarantine_agent_record(rec, record_owner, A.persist_error)
			if qpath then
				record_quarantine_path(qpath)
			end
			if qerr and not A.quarantine_error then
				A.quarantine_error = qerr
			end
			table.insert(A.pending, { record = rec, owner = record_owner, quarantined = qpath ~= nil })
			return nil, A.persist_error
		end
		local quarantined = false
		local initial_model_quarantined = false
		if #A.pending == 0 then
			local ok, err, q_now, q_init_now, persisted_id = write_record(rec, nil, record_owner, false, false)
			if ok then
				return true, nil, persisted_id
			end
			quarantined = q_now == true
			initial_model_quarantined = q_init_now == true
			if not A.persist_error then
				local notice = require("mare_lib").format_save_error(
					err,
					A.quarantine_paths or A.quarantine_path,
					A.quarantine_error
				)
				ctx.bus:emit("host/notice", { level = "warning", text = notice })
				if mare.ui.connected() then
					mare.ui.notify(notice, "warning")
				end
			end
			A.persist_error = err
		else
			local qpath, qerr = quarantine_agent_record(rec, record_owner, A.persist_error)
			if qpath then
				record_quarantine_path(qpath)
			end
			if qerr and not A.quarantine_error then
				A.quarantine_error = qerr
			end
			quarantined = qpath ~= nil
		end
		table.insert(A.pending, {
			record = rec,
			owner = record_owner,
			quarantined = quarantined,
			initial_model_quarantined = initial_model_quarantined,
		})
		return nil, A.persist_error
	end

	local function message_item_type(msg)
		if type(msg) == "table" and msg.role == "user" then
			return "user_message"
		elseif type(msg) == "table" and msg.role == "assistant" then
			return "assistant_message"
		end
		return "custom"
	end

	local function project_for_persist(msg)
		if mare.projection.active() then
			local msg_turn_id = type(msg) == "table" and msg.turn_id or nil
			local to_project = msg
			if msg_turn_id ~= nil then
				to_project = table.copy(msg)
				to_project.turn_id = nil
			end
			local out, lossy = mare.projection.project(to_project, { lossy = true })
			if lossy and mare.ui and mare.ui.notify then
				mare.ui.notify("session entry contained non-UTF-8 bytes; replaced lossily with U+FFFD", "warning")
			end
			if msg_turn_id ~= nil and type(out) == "table" then
				out.turn_id = type(msg_turn_id) == "string" and (mare.projection.project(msg_turn_id, { lossy = true }))
					or msg_turn_id
			end
			return out
		end
		return msg
	end

	--- Persist a message to the session journal without publishing it into `A.messages` yet.
	--- Returns a persisted handle that `publish_message` commits in ordered call/turn sequence.
	local function persist_message(msg, opts)
		msg = project_for_persist(msg)
		local held <close> = hold_write()
		local record_owner = owner()
		local id = (opts and opts.id) or mare.crypto.random_hex(8)
		-- a failed write doesn't stop pure conversation, but no tool runs until the missing
		-- records are written again (checked in run_tool)
		local saved, _, persisted_id = persist({ kind = "message", id = id, msg = msg }, record_owner)
		if not same_owner(record_owner, owner()) then
			return nil, "session changed while a message was being persisted"
		end
		local entry_id = saved and (persisted_id or id) or nil
		return {
			msg = msg,
			record_owner = record_owner,
			saved = saved == true,
			entry_id = entry_id,
			id = id,
			opts = opts,
		}
	end

	--- Publish a previously persisted message into `A.messages`, `A.run_msgs`, and item events.
	local function publish_message(persisted)
		if not persisted or not same_owner(persisted.record_owner, owner()) then
			return nil, "session changed while a message was being persisted", false
		end
		local msg = persisted.msg
		local entry_id = persisted.entry_id
		local opts = persisted.opts
		local event_id = (opts and opts.id) or entry_id or persisted.id
		table.insert(A.messages, msg)
		if A.run_msgs then
			-- messages of the current run, tracked by identity (robust to mid-run compaction)
			table.insert(A.run_msgs, msg)
		end
		if msg.role ~= "tool_result" then
			local turn_id = A.busy and A.turn_id or nil
			local item_status = (msg.stop_reason == "aborted" and "cancelled")
				or (msg.stop_reason == "error" and "failed")
				or "completed"
			local item = {
				type = message_item_type(msg),
				entry_id = entry_id,
				kind = type(msg.custom_type) == "string" and msg.custom_type or nil,
				message = msg,
			}
			if not (opts and opts.message_started) then
				ctx.bus:emit("item/started", {
					turn_id = turn_id,
					item_id = event_id,
					item = { type = item.type, entry_id = entry_id, message = msg },
				})
			end
			ctx.bus:emit("item/completed", {
				turn_id = turn_id,
				item_id = event_id,
				status = item_status,
				item = item,
			})
		end
		return true, nil, persisted.saved, entry_id
	end

	local function append_with_status(msg, opts)
		local persisted, err = persist_message(msg, opts)
		if not persisted then
			return nil, err, false
		end
		return publish_message(persisted)
	end

	local function append(msg, opts)
		local ok, err, _, entry_id = append_with_status(msg, opts)
		return ok, err, entry_id
	end

	-- calls left without a result (crash, kill, dispose mid-run): plugins/lua/agent/orphans.lua
	local orphans_impl
	local function orphans()
		orphans_impl = orphans_impl
			or require("agent.orphans")({ ctx = ctx, A = A, append = append, follow = D.follow })
		return orphans_impl
	end

	return {
		flush_pending = flush_pending,
		persist = persist,
		append = append,
		append_with_status = append_with_status,
		persist_message = persist_message,
		publish_message = publish_message,
		orphans = orphans,
	}
end
