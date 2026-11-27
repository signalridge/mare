-- agent.service_session: session revision, adoption, resume, queue, and state methods on the `agent` service.
return function(S)
	local ctx, cfg, A, persist, D, agent = S.ctx, S.cfg, S.A, S.persist, S.D, S.agent

	-- The token belongs to this provider instance. Snapshot the raw table graph because
	-- messages() exposes mutable tables, including nested message content.
	-- Identity and contents are both checked; cycles and shared tables are supported.
	local revisions = setmetatable({}, { __mode = "k" })
	local function roots()
		return {
			A.session_id,
			A.messages,
			A.tool_states,
			A.resume_request,
			A.steer,
			A.follow,
			A.pending,
			A.notes_revision or 0,
			A.persist_error,
			A.persist_inflight,
		}
	end
	local table_revision = require("mare_lib.table_revision")
	function agent.session_revision(_, _)
		local current = roots()
		local token = {}
		revisions[token] = { roots = current, graph = table_revision.snapshot(current) }
		return token
	end
	function agent.check_session_revision(_, _, token)
		if #A.pending > 0 or A.persist_error or A.persist_inflight > 0 then
			return false
		end
		local revision = revisions[token]
		if not revision then
			return false
		end
		local current = roots()
		for i = 1, #current do
			if not rawequal(current[i], revision.roots[i]) then
				return false
			end
		end
		return table_revision.unchanged(revision.graph)
	end
	-- Session publication: deliberately synchronous and event-free. The session plugin
	-- emits messages-replaced after its state and writer have been installed together.
	function agent.adopt_session(_, _, snapshot)
		if mare.exec.current() then
			error("agent session publication is unavailable inside a tool call", 0)
		end
		if
			type(snapshot) ~= "table"
			or type(snapshot.session_id) ~= "string"
			or type(snapshot.messages) ~= "table"
			or type(snapshot.tool_states) ~= "table"
		then
			error("invalid prepared agent session", 0)
		end
		local queue = snapshot.queue
		if queue ~= nil and type(queue) ~= "table" then
			error("invalid session queue", 0)
		end
		local steer, queued = queue and queue.steer or {}, queue and queue.follow_up or {}
		local notes = queue and queue.notes or {}
		if type(steer) ~= "table" or type(queued) ~= "table" or type(notes) ~= "table" then
			error("invalid session queue", 0)
		end
		if not agent.check_session_revision(nil, nil, snapshot.revision) then
			require("mare_lib").fail("stale_session", "agent context changed during session preparation")
		end
		-- Build everything that can fail before the first mutation.
		local follow = {}
		for _, t in ipairs(steer) do
			follow[#follow + 1] = t
		end
		for _, t in ipairs(queued) do
			follow[#follow + 1] = t
		end
		A.session_id = snapshot.session_id
		A.session_seq = (A.session_seq or 0) + 1
		if S.on_session_change then
			S.on_session_change()
		end
		A.messages = snapshot.messages
		A.tool_states = snapshot.tool_states
		A.resume_request = nil
		A.steer, A.follow = {}, follow
		-- notifications not yet delivered stay pending for the adopted session's next request
		A.notes, A.wake = mare.json.array(require("mare_lib").copy_deep(notes)), false
		A.notes_revision = (A.notes_revision or 0) + 1
		A.next_turn, A.pending_custom = {}, {}
		if S.on_restore then
			S.on_restore()
		end
	end

	function agent.set_messages(_, _, msgs, opts)
		A.session_seq = (A.session_seq or 0) + 1
		A.messages = msgs or {}
		A.next_turn, A.pending_custom = {}, {}
		if not (opts and opts.notify == false) then
			ctx.bus:emit("agent/messages-replaced", { messages = A.messages })
		end
	end
	--- restore recorded tool call states (session resume): orphan calls get their outcome
	function agent.set_tool_states(_, _, states)
		A.tool_states = states or {}
	end
	function agent.set_session_id(_, _, id)
		local changed = A.session_id ~= id
		A.session_id = id
		A.session_seq = (A.session_seq or 0) + 1
		if changed and S.on_session_change then
			S.on_session_change()
		end
	end
	--- restore a request that never got a reply (session resume after a crash
	--- four): record `aborted`, count the attempt and persist it first, then retry with backoff
	--- (TUI: automatically once idle; other frontends: on continue). Past `request_retries` it
	--- stays "request interrupted" for the user. Half-written tool calls / thinking were never
	--- stored, so nothing of it runs or is resent. opts.clean: the session was closed cleanly
	--- after the marker (shutdown mid-request) — not a crash, so the attempt isn't counted
	--- .
	function agent.set_pending_request(self, caller, rec, opts)
		if type(rec) ~= "table" or type(rec.request_id) ~= "string" then
			A.resume_request = nil
			return
		end
		local prev = math.floor(tonumber(rec.attempt) or 0)
		local clean = type(opts) == "table" and opts.clean == true
		local attempt = clean and prev or prev + 1
		local function notify_reset(ok, reset_path)
			if ok and reset_path then
				local note = ("crash guard state was corrupt and reset (backup: %s); crash counts were cleared"):format(
					tostring(reset_path)
				)
				mare.log("warn", note)
				if mare.ui and mare.ui.notify then
					mare.ui.notify(note, "warning")
				end
			end
		end
		if clean then
			-- the session closed cleanly: a crash-guard mark still left for this request is no
			-- crash (the next begin must not count one); earlier real crashes keep their count
			notify_reset(pcall(mare.crash.release, "req:" .. rec.request_id))
		end
		local ok, err = persist({
			kind = "request-aborted",
			durable = true,
			payload = { request_id = rec.request_id, attempt = prev },
		})
		if not ok then
			return nil, err
		end
		if attempt > cfg.request_retries then
			A.resume_request = nil
			notify_reset(pcall(mare.crash.finish, "req:" .. rec.request_id))
			ctx.bus:emit("host/notice", {
				level = "warning",
				text = ("the last request was interrupted %d times; not retried automatically — /continue or a new prompt"):format(
					prev
				),
			})
			return
		end
		ok, err =
			persist({ kind = "request", durable = true, payload = { request_id = rec.request_id, attempt = attempt } })
		if not ok then
			return nil, err
		end
		A.resume_request = { request_id = rec.request_id, attempt = attempt }
		ctx.bus:emit("agent/request-resumed", { request_id = rec.request_id, attempt = attempt, clean = clean or nil })
		if mare.mode == "tui" then
			local delay = math.min(cfg.request_retry_base_ms * 2 ^ (attempt - 1), cfg.request_retry_max_ms)
			local expected_seq = A.session_seq
			local expected_sid = A.session_id
			local expected_req = rec.request_id
			local expected_attempt = attempt
			ctx:spawn(function()
				mare.task.sleep(delay)
				if
					A.session_seq == expected_seq
					and A.session_id == expected_sid
					and A.resume_request
					and A.resume_request.request_id == expected_req
					and A.resume_request.attempt == expected_attempt
					and not A.busy
					and agent.interrupted(self, caller)
				then
					agent.continue(self, caller)
				end
			end)
		end
	end
	function agent.pending_request(_, _)
		return A.resume_request
	end
	--- restore queued inputs (session resume after a crash): they run as follow-ups
	function agent.set_queued(_, _, q)
		D.inputs.restore(q)
		if S.on_restore then
			S.on_restore()
		end
	end

	local function queue_image_meta(img, include_bytes)
		local safe = mare.projection.project(img)
		local mime = (type(safe.mime) == "string" and safe.mime ~= "" and safe.mime)
			or (type(safe.mime_type) == "string" and safe.mime_type ~= "" and safe.mime_type)
			or "image/png"
		local data = include_bytes and type(safe.data) == "string" and safe.data ~= "" and safe.data or nil
		return {
			mime = mime,
			path = type(safe.path) == "string" and safe.path or nil,
			size = type(safe.size) == "number" and math.floor(safe.size) or (data and #data or nil),
			data = data,
		}
	end

	local function queue_view_content(blocks, include_bytes)
		if type(blocks) ~= "table" or #blocks == 0 then
			return nil
		end
		local out = {}
		for _, block in ipairs(blocks) do
			if type(block) == "table" then
				if block.type == "image" then
					local meta = queue_image_meta(block, include_bytes)
					meta.type = "image"
					out[#out + 1] = meta
				else
					out[#out + 1] = mare.projection.project(block)
				end
			end
		end
		return mare.json.array(out)
	end

	local function queue_view_item(lane, item, include_bytes)
		local safe_text, safe_images =
			D.inputs.project_entry_fields(type(item.text) == "string" and item.text or "", item.images, nil)
		local images = {}
		for _, img in ipairs(safe_images or {}) do
			if type(img) == "table" then
				images[#images + 1] = queue_image_meta(img, include_bytes)
			end
		end
		local projected_ctx = nil
		if item.input_context ~= nil then
			projected_ctx = mare.projection.project(item.input_context)
		end
		return {
			input_id = tostring(item.input_id or ""),
			lane = lane,
			text = type(safe_text) == "string" and safe_text or "",
			content = queue_view_content(item.content, include_bytes),
			images = mare.json.array(images),
			input_context = projected_ctx,
		}
	end

	local function queue_view_items(entries, deliveries, include_bytes)
		local items = {}
		for i, item in ipairs(entries) do
			items[#items + 1] = queue_view_item(deliveries[i], item, include_bytes)
		end
		return items
	end
	-- published on D rather than on D.inputs so wiring it does not load the cold inputs module
	D.queue_view_items = queue_view_items

	function agent.list_queue(_, _)
		local entries, deliveries = D.inputs.list_entries(D.inputs.snapshot())
		return { data = mare.json.array(queue_view_items(entries, deliveries)) }
	end
	function agent.delete_queued(_, _, input_id)
		local removed = D.inputs.mutate(function(q)
			local item = D.inputs.remove_entry(q, input_id)
			if item then
				return true, true
			end
			return false, false
		end)
		if not removed then
			require("mare_lib").fail("agent.unknown_input", "queue item not found: " .. tostring(input_id))
		end
		return {}
	end
	function agent.prepare_commit(_, _, q, messages, turn_id)
		return D.inputs.prepare(q, messages, turn_id)
	end
	local function agent_update_queued(input_id, opts)
		local lib = require("mare_lib")
		if type(opts) ~= "table" or (opts.input == nil and opts.lane == nil) then
			lib.fail("invalid_arguments", "update_queued requires `input` or `lane`")
		end
		if opts.lane ~= nil and opts.lane ~= "steer" and opts.lane ~= "follow_up" then
			lib.fail("invalid_arguments", "lane must be `steer` or `follow_up`")
		end
		local next_text, next_images, next_markers, next_content
		if opts.input ~= nil then
			next_text, next_images, next_markers, next_content = D.validate.normalize_input(opts.input, opts)
			if
				next_text:match("^%s*$")
				and not (next_images and #next_images > 0)
				and not (next_content and #next_content > 0)
			then
				lib.fail("invalid_arguments", "input must not be empty")
			end
		end
		local updated = D.inputs.mutate(function(q)
			local item, kind, idx = D.inputs.find_entry(q, input_id)
			if not item then
				return nil, false
			end
			if opts.input ~= nil then
				item.text, item.images, item.image_markers =
					D.inputs.project_entry_fields(next_text, next_images, next_markers)
				item.content = next_content
			end
			local lane = kind.delivery
			if opts.lane ~= nil then
				local target_key = opts.lane == "steer" and "steer" or "follow_up"
				if target_key ~= kind.key then
					table.remove(q[kind.key], idx)
					q[target_key][#q[target_key] + 1] = item
				end
				lane = opts.lane
			end
			item.revision = (item.revision or 1) + 1
			return queue_view_item(lane, item), true
		end)
		if not updated then
			lib.fail("agent.unknown_input", "queue item not found: " .. tostring(input_id))
		end
		return updated
	end
	function agent.update_queued(_, _, input_id, opts)
		if opts ~= nil and type(opts) ~= "table" then
			require("mare_lib").fail("invalid_arguments", "update_queued options must be a table")
		end
		return agent_update_queued(input_id, opts)
	end
	--- the persistence fault: the error of the session write that
	--- failed, nil once the missing records are written again
	function agent.persist_error(_, _)
		return A.persist_error or (A.persist_inflight > 0 and "session persistence in progress" or nil)
	end
	local in_extras = false
	local function current_activity()
		if in_extras then
			return { extra = {}, extra_errors = {} }
		end
		in_extras = true
		local ok, extra, extra_errors = pcall(D.activity.extras)
		in_extras = false
		if not ok then
			error(extra, 0)
		end
		return {
			extra = mare.projection.project(extra),
			extra_errors = mare.projection.project(extra_errors),
		}
	end

	function agent.get(_, _)
		local handled, routed = S.route_frontend("get")
		if handled then
			if type(routed) == "table" then
				routed.activity = current_activity()
			end
			return routed
		end
		local m = ctx.llm:current()
		return {
			busy = A.busy,
			turn_id = A.busy and A.turn_id or nil,
			session_id = A.session_id,
			queued = #A.steer + #A.follow,
			message_count = #A.messages,
			model = m and m.ref or nil,
			effort = ctx.llm:effort(),
			turn_cancelled = A.token ~= nil and A.token:is_cancelled() or false,
			interrupted = agent.interrupted(nil, nil),
			activity = current_activity(),
		}
	end
end
