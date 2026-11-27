-- agent.inputs: user / notification messages, the steer and follow-up queues; part of the `agent` plugin (plugins/agent.lua), loaded at the start of its apply.
return function(D)
	local ctx, A = D.ctx, D.A

	local function text_msg(role, text)
		return {
			role = role,
			content = mare.json.array({ { type = "text", text = text } }),
			timestamp = mare.task.now(),
		}
	end

	local revision = 0
	local tx, rx = mare.task.channel()
	tx:send(true)
	local trusted_notifications = setmetatable({}, { __mode = "k" })
	local function copy(value)
		if type(value) ~= "table" then
			return value
		end
		local result = {}
		for key, item in pairs(value) do
			result[key] = copy(item)
		end
		if mare.json.is_array(value) then
			mare.json.array(result)
		end
		return result
	end
	local function array_copy(value)
		if value == nil then
			return nil
		end
		return mare.json.array(copy(value))
	end
	local function normalize(q)
		q.notes = mare.json.array(q.notes or {})
		for _, kind in ipairs({ "steer", "follow_up" }) do
			for _, entry in ipairs(q[kind] or {}) do
				if entry.images then
					mare.json.array(entry.images)
				end
				if entry.content then
					mare.json.array(entry.content)
				end
				if entry.image_markers then
					mare.json.array(entry.image_markers)
				end
			end
		end
	end
	local function typed_err(code, message, phase)
		return setmetatable({ code = code, message = tostring(message), phase = phase }, {
			__tostring = function(t)
				return t.message
			end,
		})
	end
	local function lock(cancel)
		if cancel then
			if cancel:is_cancelled() then
				error(typed_err("cancelled", "cancelled", "pre_commit"), 0)
			end
			local winner = mare.task.select(function()
				rx:recv()
				return true
			end, function()
				cancel:wait()
				return false
			end)
			if winner == 2 then
				error(typed_err("cancelled", "cancelled", "pre_commit"), 0)
			end
		else
			rx:recv()
		end
		A.persist_inflight = A.persist_inflight + 1
		return setmetatable({}, {
			__close = function()
				A.persist_inflight = A.persist_inflight - 1
				tx:send(true)
			end,
		})
	end
	local function snapshot()
		return { steer = copy(A.steer), follow_up = copy(A.follow), notes = array_copy(A.notes) }
	end
	local function prepare(q, messages, turn_id)
		require("mare_lib").validate_inbox(q)
		q, messages = copy(q), copy(messages)
		normalize(q)
		local expected = revision
		local used = false
		local function valid()
			return revision == expected and (not turn_id or (A.busy and A.turn_id == turn_id))
		end
		return function()
			assert(not used and valid(), "input publication became stale")
			used = true
			A.steer, A.follow, A.notes = q.steer, q.follow_up, q.notes
			A.notes_revision = (A.notes_revision or 0) + 1
			revision = revision + 1
			for _, message in ipairs(messages) do
				A.messages[#A.messages + 1] = message
				if A.run_msgs then
					A.run_msgs[#A.run_msgs + 1] = message
				end
			end
		end,
			valid,
			messages
	end
	local function owner(turn_id)
		local info = D.follow.session():info()
		return { session_id = info.session_id, session_epoch = info.session_epoch, turn_id = turn_id }
	end
	local function project_content(value)
		if type(value) == "table" then
			return mare.projection.project(array_copy(value))
		elseif type(value) == "string" then
			return mare.projection.project(value)
		end
		return mare.json.array()
	end
	local function try_commit(q, messages, cancel, expected, operation, sess_override)
		expected.cancel = cancel
		local sess = sess_override or D.follow.session()
		local result, err, c3, c4, c5, c6 =
			sess:commit({ version = 1, inbox = q, messages = messages, operation = operation }, expected)
		if not result then
			local err_code = c3 or c5 or "persist_failed"
			local commit_phase = c4 or c6 or "pre_commit"
			return nil, typed_err(err_code, err, commit_phase)
		end
		if operation ~= nil then
			local op = type(result) == "table" and result.operation or nil
			local op_res = type(op) == "table" and op.result or nil
			local expected_id = operation.result and operation.result.input_id or nil
			local need_input_id = operation.kind == "agent.enqueue" or expected_id ~= nil
			if
				type(op) ~= "table"
				or op.request_id ~= operation.request_id
				or op.kind ~= operation.kind
				or op.fingerprint ~= operation.fingerprint
				or type(op_res) ~= "table"
				or (need_input_id and (type(op_res.input_id) ~= "string" or op_res.input_id == ""))
				or (expected_id ~= nil and op_res.input_id ~= expected_id)
			then
				return nil,
					typed_err(
						"missing_operation_receipt",
						"session commit did not confirm atomic operation receipt",
						"committed"
					)
			end
		end
		if D.ctx and D.ctx.kernel and D.ctx.kernel.mark_call_committed then
			local ok_mark, mark_err = pcall(D.ctx.kernel.mark_call_committed, D.ctx.kernel)
			if not ok_mark then
				return nil, typed_err("post_commit_failed", mark_err, "committed")
			end
		end
		return result, nil
	end
	local function commit(q, messages, cancel, expected, operation, sess_override)
		local result, err = try_commit(q, messages, cancel, expected, operation, sess_override)
		if not result then
			error(err, 0)
		end
		return result
	end
	local pub_queue = {}
	local publishing = false
	local function build_queue_items(q)
		local out = mare.json.array()
		for _, lane in ipairs({ "steer", "follow_up" }) do
			for _, entry in ipairs(q[lane] or {}) do
				out[#out + 1] = {
					input_id = entry.input_id,
					lane = lane,
					text = entry.text,
				}
			end
		end
		return out
	end
	local function project_lane(items, kind)
		items = items or {}
		if D.queue_view_items then
			local deliveries = {}
			for i = 1, #items do
				deliveries[i] = kind
			end
			return mare.json.array(D.queue_view_items(items, deliveries))
		end
		return array_copy(items)
	end
	local function enqueue_pub(queue_snapshot, messages, ids, action)
		pub_queue[#pub_queue + 1] = { queue = queue_snapshot, messages = messages, ids = ids, action = action }
	end
	local function flush_pub()
		if publishing then
			return
		end
		publishing = true
		local guard <close> = setmetatable({}, {
			__close = function()
				publishing = false
			end,
		})
		while #pub_queue > 0 do
			local batch = table.remove(pub_queue, 1)
			if batch.queue ~= nil then
				local q = batch.queue
				local payload = mare.projection.project({
					items = build_queue_items(q),
					action = batch.action,
				})
				ctx.bus:emit("queue/changed", payload)
			end
			if batch.messages then
				local turn_id = A.busy and A.turn_id or nil
				for idx, message in ipairs(batch.messages) do
					local entry_id = batch.ids and batch.ids[idx] or nil
					local item_id = entry_id or mare.crypto.random_hex(8)
					local item_type = (message.role == "user" and "user_message")
						or (message.role == "assistant" and "assistant_message")
						or "custom"
					local item = {
						type = item_type,
						entry_id = entry_id,
						message = message,
					}
					ctx.bus:emit("item/started", { turn_id = turn_id, item_id = item_id, item = item })
					ctx.bus:emit("item/completed", {
						turn_id = turn_id,
						item_id = item_id,
						status = "completed",
						item = item,
					})
				end
			end
		end
	end
	local function mutate(change, turn_id, on_commit, cancel, expected_owner, sess_override)
		if cancel and cancel:is_cancelled() then
			error(typed_err("cancelled", "cancelled", "pre_commit"), 0)
		end
		local expected = expected_owner
				and {
					session_id = expected_owner.session_id,
					session_epoch = expected_owner.session_epoch,
					turn_id = turn_id,
				}
			or owner(turn_id)
		local result, did_commit = nil, false
		do
			local held <close> = lock(cancel)
			if cancel and cancel:is_cancelled() then
				error(typed_err("cancelled", "cancelled", "pre_commit"), 0)
			end
			local current = owner()
			if current.session_id ~= expected.session_id or current.session_epoch ~= expected.session_epoch then
				error(typed_err("stale_session", "input owner changed before queue admission", "pre_commit"), 0)
			end
			local q = snapshot()
			local prev_count = #q.steer + #q.follow_up
			local changed, operation, messages
			result, changed, operation, messages = change(q)
			messages = messages or {}
			local published_messages, published_ids
			if changed ~= false or operation ~= nil or #messages > 0 then
				local committed = commit(q, messages, cancel, expected, operation, sess_override)
				published_messages = #messages > 0 and committed.messages or nil
				published_ids = #messages > 0 and committed.ids or nil
				did_commit = true
				-- notes are not queued user input: changing only them publishes no queue change
				local queue_changed = changed ~= false and changed ~= "notes"
				if queue_changed or published_messages then
					local after_q = queue_changed and snapshot() or nil
					local action = nil
					if after_q then
						local next_count = #after_q.steer + #after_q.follow_up
						action = (next_count > prev_count and "enqueued")
							or (next_count == 0 and "cleared")
							or "removed"
					end
					enqueue_pub(after_q, published_messages, published_ids, action)
				end
			end
		end
		if did_commit then
			if on_commit then
				local ok_cb, cb_err = pcall(on_commit)
				if not ok_cb then
					mare.log("warn", "agent input commit callback failed: " .. tostring(cb_err))
				end
			end
			local ok_pub, pub_err = pcall(flush_pub)
			if not ok_pub then
				mare.log("warn", "agent input publication failed: " .. tostring(pub_err))
			end
		end
		return result
	end

	local function drain(queue)
		local out = {}
		while #queue > 0 do
			out[#out + 1] = table.remove(queue, 1)
		end
		return out
	end

	local QUEUE_KINDS = {
		{ key = "steer", delivery = "steer", state_key = "steer" },
		{ key = "follow_up", delivery = "follow_up", state_key = "follow" },
	}

	local function find_entry(q, id)
		for _, kind in ipairs(QUEUE_KINDS) do
			for i, item in ipairs(q[kind.key] or {}) do
				if item.input_id == id then
					return item, kind, i
				end
			end
		end
		return nil
	end

	local function list_entries(q, clear)
		local entries, deliveries = {}, {}
		for _, kind in ipairs(QUEUE_KINDS) do
			for _, item in ipairs(q[kind.key] or {}) do
				entries[#entries + 1] = item
				deliveries[#deliveries + 1] = kind.delivery
			end
			if clear then
				q[kind.key] = mare.json.array()
			end
		end
		return entries, deliveries
	end

	local function drain_entries(q)
		return list_entries(q, true)
	end

	local function drain_in_memory()
		local q = snapshot()
		local entries, deliveries = drain_entries(q)
		if #entries > 0 then
			A.steer, A.follow = q.steer, q.follow_up
			revision = revision + 1
			enqueue_pub(snapshot())
			flush_pub()
		end
		return entries, deliveries
	end

	local function remove_entry(q, id)
		local item, kind, i = find_entry(q, id)
		if item then
			table.remove(q[kind.key], i)
			return item, kind.delivery
		end
		return nil
	end

	local function build_user(text, images, content, source, peer_message, notification)
		local msg
		if type(content) == "table" and #content > 0 then
			msg = {
				role = "user",
				content = array_copy(content),
				timestamp = mare.task.now(),
			}
		else
			msg = text_msg("user", text)
			for _, img in ipairs(images or {}) do
				table.insert(msg.content, img)
			end
		end
		-- the host's control fields are attached after the content is projected: they are its
		-- classification, not user data a registered value may rewrite
		msg = mare.projection.project(msg)
		if type(source) == "string" and source ~= "" then
			msg.source = source
		end
		if peer_message == true then
			msg.peer_message = true
		end
		if notification == true then
			msg.notification = true
		end
		return copy(ctx.bus:waterfall("input/transform", msg) or msg)
	end

	-- All queued inputs carry their stable durable identity.
	local function build_queued(e)
		local msg
		if type(e.custom_message) == "table" then
			msg = mare.projection.project(copy(e.custom_message))
			msg.timestamp = mare.task.now()
		else
			msg = build_user(e.text, e.images, e.content, e.source, e.peer_message)
		end
		msg._input_id = e.input_id
		msg.input_id = e.input_id
		if e.input_attempt_id ~= nil then
			msg.input_attempt_id = e.input_attempt_id
		end
		if e.input_context ~= nil then
			msg.input_context = mare.projection.project(copy(e.input_context))
		end
		return msg
	end

	--- one user message carrying notifications (not user input: no input/transform)
	local function note_message(notes)
		local texts, ids = {}, {}
		for i, note in ipairs(notes) do
			texts[i], ids[i] = note.text, note.note_id
		end
		local msg = text_msg("user", table.concat(texts, "\n\n"))
		msg.notification = true
		trusted_notifications[msg] = ids
		return msg
	end

	--- Queue a notification for the model's next request. It is part of the inbox, so it is
	--- committed with the session and survives exit, crash and resume until delivered.
	--- `in_memory` (a session switch is replacing the inbox) keeps it out of any commit.
	local function notify_enqueue(text, in_memory)
		local note = { note_id = mare.crypto.random_hex(16), text = mare.projection.project(text) }
		local ok, err = true, nil
		if not in_memory then
			ok, err = pcall(mutate, function(q)
				q.notes[#q.notes + 1] = note
				return true, "notes"
			end)
		end
		if in_memory or not ok then
			-- no writable session (read-only, failed commit): keep it for this process; the
			-- next inbox commit stores it
			if err then
				mare.log("debug", "notification kept in memory: " .. tostring(err))
			end
			A.notes[#A.notes + 1] = note
			if not in_memory then
				A.notes_revision = (A.notes_revision or 0) + 1
			end
		end
	end

	--- The pending notifications as one message; they stay queued until `deliver_notes`.
	local function take_notes()
		return note_message(A.notes)
	end

	--- Append a `take_notes` message to the conversation and remove its notes from the inbox in
	--- one commit, so a delivered notification is never delivered again after a resume.
	local function deliver_notes(msg, cancel)
		local ids = trusted_notifications[msg]
		assert(type(ids) == "table", "not a pending notification message")
		local delivered = {}
		for _, id in ipairs(ids) do
			delivered[id] = true
		end
		mutate(function(q)
			local keep = mare.json.array()
			for _, note in ipairs(q.notes) do
				if not delivered[note.note_id] then
					keep[#keep + 1] = note
				end
			end
			q.notes = keep
			return true, "notes", nil, { msg }
		end, A.busy and A.turn_id or nil, nil, cancel)
		if #A.notes == 0 then
			A.wake = false
		end
		-- the conversation holds the committed copy of the message
		local committed = A.messages[#A.messages]
		if type(committed) == "table" and committed.notification then
			trusted_notifications[committed] = ids
		end
	end

	local function is_trusted_notification(msg)
		return type(msg) == "table" and trusted_notifications[msg] ~= nil
	end

	--- Project all queued entry fields and align `image_markers` offsets with the projected text.
	local function project_entry_fields(text, images, image_markers, content, custom_message)
		local safe_text, safe_markers = require("mare_lib").project_text_and_markers(text, image_markers, array_copy)
		return safe_text,
			array_copy(images and mare.projection.project(images)),
			safe_markers,
			array_copy(content and mare.projection.project(content)),
			custom_message and mare.projection.project(copy(custom_message)) or nil
	end

	--- steer(text, opts?) / follow_up(text, opts?) — opts.images: image blocks
	local function entry(text, opts)
		local safe_text, safe_images, safe_markers, safe_content, safe_custom = project_entry_fields(
			text,
			opts and opts.images,
			opts and opts.image_markers,
			opts and opts.content,
			opts and opts.custom_message
		)
		local projected_context = nil
		if opts and opts.input_context ~= nil then
			projected_context = mare.projection.project(copy(opts.input_context))
		end
		return {
			input_id = mare.crypto.random_hex(16),
			revision = 1,
			text = safe_text,
			images = safe_images,
			content = safe_content,
			image_markers = safe_markers,
			source = opts and opts.source or nil,
			peer_message = opts and opts.peer_message == true or nil,
			custom_message = safe_custom,
			input_context = projected_context,
		}
	end

	local function session_identity()
		local sess = D.follow.session and D.follow.session()
		local info = sess and sess:info()
		return {
			session_id = info and info.session_id or A.session_id or "",
			session_epoch = ctx.kernel:session_epoch(),
		}
	end

	local function admit_input(params)
		local attempt_id = mare.crypto.random_hex(16)
		local policy = ctx.policy --[[@as mare.PolicyService]]
		local had_voters = policy:has_voters("agent/input-ready")
		if not had_voters then
			if params.cancel and params.cancel:is_cancelled() then
				return nil, "aborted", false, "cancelled", false
			end
			return attempt_id, nil, false, nil, false
		end
		local projected_ctx = nil
		if params.input_context ~= nil then
			projected_ctx = mare.projection.project(copy(params.input_context))
		end
		local vote_content = type(params.content) == "string" and { { type = "text", text = params.content } }
			or (params.content or mare.json.array())
		local payload = {
			input_id = params.input_id,
			attempt_id = attempt_id,
			revision = params.revision or 1,
			turn_id = params.turn_id or A.turn_id,
			cancel = params.cancel,
			session = session_identity(),
			content = mare.projection.project(copy(vote_content)),
			input_context = projected_ctx,
			source = params.source,
			delivery = params.delivery,
		}
		local _, verdict = mare.task.select(function()
			return policy:decide("agent/input-ready", { payload })
		end, function()
			if params.cancel then
				params.cancel:wait()
			end
		end)
		if params.cancel and params.cancel:is_cancelled() then
			return nil, "aborted", false, "cancelled", had_voters
		end
		if not verdict or verdict.decision ~= "allow" then
			local reason = (verdict and verdict.reason) or "input denied"
			local abort = type(verdict) == "table" and verdict.abort == true
			return nil, reason, abort, "denied", had_voters
		end
		return attempt_id, nil, false, nil, had_voters
	end

	local function collect_selected(q, which, only_input_id, skip_parked)
		local selected = {}
		for _, kind in ipairs({ "steer", "follow_up" }) do
			if which == kind or which == "all" then
				for _, item in ipairs(q[kind] or {}) do
					local parked = skip_parked and D.is_input_parked and D.is_input_parked(item.input_id)
					if not parked and (not only_input_id or item.input_id == only_input_id) then
						selected[#selected + 1] = {
							entry = copy(item),
							lane = kind,
							lane_key = kind,
							revision = item.revision or 1,
						}
					end
				end
			end
		end
		return selected
	end

	local function consume(which, cancel, only_input_id, skip_parked)
		local expected = owner(A.turn_id)
		local result
		for _ = 1, 8 do
			local selected
			do
				local held <close> = lock()
				selected = collect_selected(snapshot(), which, only_input_id, skip_parked)
				if #selected == 0 then
					return {}
				end
			end

			local superseded = false
			for _, rec in ipairs(selected) do
				if cancel:is_cancelled() then
					return nil, "aborted", false, "cancelled"
				end
				local item = rec.entry
				-- Build the exact transformed candidate message outside the lock so admission sees the
				-- exact post-transform content that will be committed.
				local candidate
				if type(item.custom_message) == "table" then
					candidate = mare.projection.project(copy(item.custom_message))
					candidate.timestamp = mare.task.now()
				else
					candidate = copy(build_user(item.text, item.images, item.content, item.source, item.peer_message))
				end
				candidate.content = project_content(candidate.content)
				rec.voted_content_json = mare.json.encode(candidate.content)
				local attempt_id, deny_reason, deny_abort, deny_kind = admit_input({
					input_id = item.input_id,
					revision = rec.revision,
					turn_id = A.turn_id,
					cancel = cancel,
					content = copy(candidate.content),
					input_context = item.input_context,
					source = item.source,
					delivery = rec.lane,
				})
				if not attempt_id then
					if deny_kind == "cancelled" or cancel:is_cancelled() then
						return nil, "aborted", false, "cancelled"
					end
					-- Revalidate item identity and revision under lock before acting on a denial/abort.
					local still_current = false
					do
						local held <close> = lock()
						if cancel:is_cancelled() then
							return nil, "aborted", false, "cancelled"
						end
						local cur = find_entry(snapshot(), item.input_id)
						if cur and (cur.revision or 1) == rec.revision then
							still_current = true
						end
					end
					if still_current then
						return nil, deny_reason, deny_abort, deny_kind
					end
					superseded = true
					break
				end
				candidate.input_attempt_id = attempt_id
				candidate._input_id = item.input_id
				candidate.input_id = item.input_id
				candidate.turn_id = A.turn_id
				if item.input_context ~= nil then
					candidate.input_context = mare.projection.project(copy(item.input_context))
				end
				rec.candidate = candidate
			end

			if not superseded then
				local committed_now = false
				do
					local held <close> = lock()
					if cancel:is_cancelled() then
						return nil, "aborted", false, "cancelled"
					end
					local q = snapshot()
					local by_id = {}
					for _, kind in ipairs(QUEUE_KINDS) do
						for _, item in ipairs(q[kind.key] or {}) do
							if item.input_id ~= nil then
								by_id[item.input_id] = { item = item, lane_key = kind.key }
							end
						end
					end
					local matches = true
					for _, rec in ipairs(selected) do
						local found = by_id[rec.entry.input_id]
						if
							not found
							or (found.item.revision or 1) ~= rec.revision
							or (which ~= "all" and found.lane_key ~= rec.lane_key)
							or mare.json.encode(rec.candidate.content) ~= rec.voted_content_json
						then
							matches = false
							break
						end
					end
					if matches then
						local selected_ids = {}
						local messages = {}
						local consumed_ids = {}
						for _, rec in ipairs(selected) do
							selected_ids[rec.entry.input_id] = true
							consumed_ids[#consumed_ids + 1] = rec.entry.input_id
							messages[#messages + 1] = copy(rec.candidate)
						end
						for _, kind in ipairs({ "steer", "follow_up" }) do
							local kept = mare.json.array()
							for _, item in ipairs(q[kind] or {}) do
								if not selected_ids[item.input_id] then
									kept[#kept + 1] = item
								end
							end
							q[kind] = kept
						end
						local err
						result, err = try_commit(q, messages, cancel, expected)
						if not result then
							if cancel:is_cancelled() or (type(err) == "table" and err.code == "cancelled") then
								return nil, "aborted", false, "cancelled"
							end
							error(err, 0)
						end
						if D.on_consume_inputs then
							D.on_consume_inputs(consumed_ids)
						end
						enqueue_pub(snapshot(), result.messages, result.ids, "consumed")
						committed_now = true
					end
				end
				if committed_now then
					local ok_pub, pub_err = pcall(flush_pub)
					if not ok_pub then
						mare.log("warn", "agent queued input publication failed: " .. tostring(pub_err))
					end
					return result.messages
				end
			end
		end
		error("queued input changed repeatedly during admission", 0)
	end

	local function has_pending(which, skip_parked)
		for _, kind in ipairs(QUEUE_KINDS) do
			if which == kind.key or which == "all" then
				for _, item in ipairs(A[kind.state_key] or {}) do
					local parked = skip_parked and D.is_input_parked and D.is_input_parked(item.input_id)
					if not parked then
						return true
					end
				end
			end
		end
		return false
	end

	local function has_input_ready_voters()
		return ctx.policy:has_voters("agent/input-ready")
	end

	return {
		text_msg = text_msg,
		restore = function(q)
			require("mare_lib").validate_inbox(q)
			q = copy(q)
			normalize(q)
			A.steer, A.follow, A.notes = mare.json.array(), list_entries(q), q.notes
			A.notes_revision = (A.notes_revision or 0) + 1
			A.session_seq = (A.session_seq or 0) + 1
			revision = revision + 1
		end,
		project_content = project_content,
		mutate = mutate,
		snapshot = snapshot,
		prepare = prepare,
		admit_input = admit_input,
		has_pending = has_pending,
		has_input_ready_voters = has_input_ready_voters,
		consume = consume,
		drain = drain,
		find_entry = find_entry,
		list_entries = list_entries,
		drain_entries = drain_entries,
		drain_in_memory = drain_in_memory,
		remove_entry = remove_entry,
		build_user = build_user,
		build_queued = build_queued,
		note_message = note_message,
		notify_enqueue = notify_enqueue,
		take_notes = take_notes,
		deliver_notes = deliver_notes,
		is_trusted_notification = is_trusted_notification,
		entry = entry,
		project_entry_fields = project_entry_fields,
	}
end
