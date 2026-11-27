-- agent.loop_request: request assembly, stream coalescing, request execution, and recovery/outcome handling.
return function(D)
	local ctx, cfg, A = D.ctx, D.cfg, D.A
	local model_snapshot = require("agent.model_snapshot")
	local outcome = require("agent.outcome")
	local orphans, append, persist = D.persist.orphans, D.persist.append, D.persist.persist

	local function add_usage(S, msg, fallback_model)
		local u = type(msg) == "table" and msg.usage or nil
		if type(u) ~= "table" then
			if type(msg) == "table" and not msg.subscription and msg.stop_reason ~= "error" then
				S.usage_cost_incomplete = true
				S.usage.cost = nil
				S.usage.cost_source = "unknown"
			end
			return
		end
		for k, v in pairs(u) do
			if k ~= "cost" and type(v) == "number" then
				S.usage[k] = (S.usage[k] or 0) + v
			end
		end
		local used_model = (
			type(msg.provider_id) == "string"
			and type(msg.model_id) == "string"
			and ctx.llm:resolve(msg.provider_id .. "/" .. msg.model_id)
		) or fallback_model
		local req_cost, req_source = require("mare_lib").usage_cost(used_model, u, msg.subscription)
		if req_cost == nil or S.usage_cost_incomplete then
			S.usage_cost_incomplete = true
			S.usage.cost = nil
			S.usage.cost_source = "unknown"
		else
			S.usage.cost = (S.usage.cost or 0) + req_cost
			local prev = S.usage.cost_source
			if not prev then
				S.usage.cost_source = req_source
			elseif prev ~= req_source and prev ~= "unknown" then
				S.usage.cost_source = "mixed"
			end
		end
	end

	-- the tool list is stable: it's re-read between requests only when tools
	-- were added or removed (e.g. plugin reload, MCP server connect) or the model changed —
	-- one provider cache invalidation, recorded in /profile; never inside a request
	local function resolve_tool_defs(tool_cache, model, frozen_model, opt_tools)
		local gen = ctx.tools.revision and ctx.tools:revision() or 0
		if not tool_cache.defs or not model_snapshot.equal(tool_cache.model, frozen_model) or tool_cache.gen ~= gen then
			tool_cache.defs, tool_cache.model = ctx.tools:definitions(model, opt_tools), frozen_model
			tool_cache.gen = gen
			local names = {}
			for _, d in ipairs(tool_cache.defs or {}) do
				if not d.defer_loading then
					names[#names + 1] = d.name or ""
				end
			end
			local sig = table.concat(names, ",")
			if A.tools_sig and A.tools_sig ~= sig then
				mare.profile.mark("prompt cache invalidated: tool list changed")
			end
			A.tools_sig = sig
		end
		return tool_cache.defs
	end

	-- A step that could not be admitted or committed ends the run without a request.
	local function fail_step(S, err, err_kind)
		if err_kind == "cancelled" or S.token:is_cancelled() then
			S.interrupt()
		else
			S.run_error = err
		end
	end

	local function build_request(S, opts, tool_cache, reservation)
		local lib = require("mare_lib")
		-- Hooks and schema callbacks receive an isolated model value; no request may
		-- silently change the model after its declarations have been constructed.
		local model = model_snapshot.copy(ctx.llm:current())
		local frozen_model = model_snapshot.copy(model)
		-- a sub-agent's prompt is assembled for it
		local agent = mare.mode == "child" and mare.args and mare.args.agent_label or nil
		local context = D.follow.context()
		local system = opts.system or (context and context:system_prompt(model, agent)) or ""
		local history = orphans().repair(lib.copy_deep(A.messages))
		local wf = ctx.bus:waterfall("context/transform", { messages = history, model = model })
		if type(wf) == "table" and type(wf.messages) == "table" then
			history = wf.messages
		end
		-- a context listener stopped the run (agent:halt, e.g. compaction can't free room):
		-- nothing is sent
		if A.halt then
			S.run_error = A.halt
			return nil, nil
		end
		-- Commit the step's participant state and disclosures once, installing them into the
		-- active conversation before agent/message and agent/step-committed fire.
		local accepted, step_error, step_id, disclosures = D.steps.accept(
			S.turns,
			S.token,
			A.turn_id,
			history,
			reservation,
			function(committed)
				for _, message in ipairs(committed or {}) do
					A.messages[#A.messages + 1] = message
					if A.run_msgs then
						A.run_msgs[#A.run_msgs + 1] = message
					end
					history[#history + 1] = message
				end
			end,
			opts.tools
		)
		if not accepted then
			fail_step(S, step_error)
			return nil, nil
		end
		local baseline_history = lib.copy_deep(history)
		local req = {
			model = model,
			system = system,
			messages = lib.copy_deep(history),
			tools = resolve_tool_defs(tool_cache, model, frozen_model, opts.tools),
			cancel = S.token,
			session_id = A.session_id,
			turn_id = A.turn_id,
			step_id = step_id,
			turn = S.turns,
			output_schema = S.compiled_output_schema and S.compiled_output_schema:value() or nil,
		}
		req = ctx.bus:waterfall_checked("request/build", req) or req
		if req.model ~= model or not model_snapshot.equal(req.model, frozen_model) then
			error("request/build changed the request model after tool declaration")
		end
		-- plugin tool constraints narrow the declarations before they are bound (llm:constrain_tools);
		-- they see the request's purpose, the conversation's own
		req.purpose = req.purpose or "agent"
		req = ctx.llm:constrain_request(req)
		local declared_tools, request_tools = ctx.tools:bind_declarations(req.tools)
		local decl_holder = { tools = declared_tools }
		req.tools = request_tools
		local function prepare_parameters(prepared)
			local base = lib.output_limit(prepared.max_tokens, prepared.model.max_output)
			if not base then
				error("invalid request max_tokens or model max_output", 0)
			end
			local overlay = { body = { max_tokens = base }, headers = {} }
			overlay.operations, overlay.effort, overlay.route, overlay.reserved =
				ctx.llm:apply_params(prepared, overlay, { prepare = true })
			prepared.params_overlay = overlay
		end
		local function prepare(prepared, original)
			prepare_parameters(prepared)
			local compact = D.follow.compact()
			if compact and lib.callable(compact.prepare_request) then
				local result = compact:prepare_request(prepared, original)
				prepared.messages = result.messages
				return result.compacted
			end
			return false
		end
		-- Count each occurrence in the prepared message tree. Image data is already base64;
		-- derive its binary length and round back up to the encoded wire size.
		local function image_bytes(value, stack)
			if type(value) ~= "table" or stack[value] then
				return 0
			end
			stack[value] = true
			local total = 0
			if value.type == "image" and type(value.data) == "string" then
				local padding = value.data:match("(=*)$") or ""
				local bytes = math.max(0, math.floor(#value.data * 3 / 4) - #padding)
				total = math.max(#value.data, 4 * math.ceil(bytes / 3))
			else
				for _, child in pairs(value) do
					total = total + image_bytes(child, stack)
				end
			end
			stack[value] = nil
			return total
		end
		local function replace_images(value, replacements, stack)
			if type(value) ~= "table" or stack[value] or value.role == "assistant" then
				return
			end
			stack[value] = true
			for key, child in pairs(value) do
				if
					type(child) == "table"
					and child.type == "image"
					and type(child.data) == "string"
					and replacements[child.data]
				then
					value[key] = lib.copy_deep(replacements[child.data])
				else
					replace_images(child, replacements, stack)
				end
			end
			stack[value] = nil
		end
		local function preflight_images(prepared)
			local limit = tonumber(prepared.model.max_image_wire_bytes) or cfg.max_image_wire_bytes or 20 * 1024 * 1024
			local compact = D.follow.compact()
			if
				limit <= 0
				or image_bytes(prepared.messages, {}) <= limit
				or not compact
				or not lib.callable(compact.offload_images)
			then
				return nil
			end
			local before = lib.copy_deep(A.messages)
			local n, err = compact:offload_images({ keep = 0 })
			if not n or n <= 0 then
				ctx.bus:emit("host/notice", {
					level = "warning",
					text = "Image preflight could not offload images: " .. tostring(err or "nothing to offload"),
				})
				return nil
			end
			local replacements = {}
			for i, old in ipairs(before) do
				local new = A.messages[i]
				for j, block in ipairs(old.content or {}) do
					local replacement = new and new.content and new.content[j]
					if
						type(block) == "table"
						and block.type == "image"
						and type(block.data) == "string"
						and type(replacement) == "table"
						and replacement.type == "text"
					then
						replacements[block.data] = replacement
					end
				end
			end
			replace_images(prepared.messages, replacements, {})
			return replacements
		end
		prepare(req, baseline_history)
		preflight_images(req)
		if A.halt then
			S.run_error = A.halt
			return nil, nil
		end
		-- Each fallback starts from the primary envelope, without accumulating previous
		-- fallback hook mutations. Functions, token objects and model descriptors retain identity.
		local function copy_request(source)
			local out = {}
			for key, value in pairs(source) do
				if type(value) == "table" and key ~= "model" and key ~= "cancel" then
					out[key] = lib.copy_deep(value)
				else
					out[key] = value
				end
			end
			return out
		end
		local request_base = copy_request(req)
		-- A fallback is a new provider request and may enable different tools or produce a
		-- different dynamic schema. Bind its actual declarations to its response, not to
		-- the primary model's request.
		req._rebuild_tool_declarations = function(fallback_req)
			local fallback_model = model_snapshot.copy(fallback_req.model)
			local frozen_fallback_model = model_snapshot.copy(fallback_model)
			local fallback_history = lib.copy_deep(A.messages)
			local incoming = fallback_req
			fallback_req = copy_request(request_base)
			fallback_req.params_overlay = nil
			for _, key in ipairs({ "request_id", "attempt", "on_attempt", "no_fallback", "_rebuild_tool_declarations" }) do
				fallback_req[key] = incoming[key]
			end
			fallback_req.model = fallback_model
			fallback_req.tools = ctx.tools:definitions(fallback_model, opts.tools)
			fallback_req = ctx.bus:waterfall_checked("request/build", fallback_req) or fallback_req
			if
				fallback_req.model ~= fallback_model
				or not model_snapshot.equal(fallback_req.model, frozen_fallback_model)
			then
				error("request/build changed the fallback model after tool declaration")
			end
			fallback_req = ctx.llm:constrain_request(fallback_req)
			if fallback_req.effort ~= nil then
				fallback_req.effort = ctx.llm:clamp_effort(fallback_model, fallback_req.effort)
			end
			local bound, wire = ctx.tools:bind_declarations(fallback_req.tools)
			fallback_req.tools, decl_holder.tools = wire, bound
			if prepare(fallback_req, fallback_history) then
				local start = lib.find_history(request_base.messages, fallback_history)
				if start then
					request_base.messages =
						lib.replace_history(request_base.messages, start, #fallback_history, A.messages)
				end
			end
			local replacements = preflight_images(fallback_req)
			if replacements then
				replace_images(request_base.messages, replacements, {})
			end
			if A.halt then
				error(A.halt, 0)
			end
			D.activity.request(fallback_req)
			return fallback_req
		end
		return req, decl_holder
	end

	-- Deltas are coalesced and emitted in chunks (≤ ~20/s), never per token: listeners
	-- (UI, RPC, ACP) see the same text with far fewer calls into Lua.
	local function create_stream_sink(model, step_id)
		local native = mare.stream.coalescer()
		local message_started = false
		local pre_id = mare.crypto.random_hex(8)
		local offsets = { text = 0, reasoning = 0, tool_arguments = 0, event = 0 }
		local stream_message = {
			role = "assistant",
			provider_id = model and model.provider or nil,
			model_id = model and model.id or nil,
		}
		local function emit_stream(ev)
			if not message_started then
				message_started = true
				ctx.bus:emit("item/started", {
					turn_id = A.turn_id,
					item_id = pre_id,
					step_id = step_id,
					item = { type = "assistant_message", message = stream_message },
				})
			end
			local t = ev.type
			if t == "text_delta" then
				local delta = ev.delta or ""
				local off = offsets.text
				offsets.text = off + #delta
				ctx.bus:emit("item/delta", {
					turn_id = A.turn_id,
					item_id = pre_id,
					kind = "text",
					delta = delta,
					offset = off,
				})
			elseif t == "reasoning_delta" then
				local delta = ev.delta or ""
				local off = offsets.reasoning
				offsets.reasoning = off + #delta
				ctx.bus:emit("item/delta", {
					turn_id = A.turn_id,
					item_id = pre_id,
					kind = "reasoning",
					delta = delta,
					offset = off,
				})
			elseif t == "tool_call_delta" then
				local delta = ev.delta or ""
				local off = offsets.tool_arguments
				offsets.tool_arguments = off + #delta
				ctx.bus:emit("item/delta", {
					turn_id = A.turn_id,
					item_id = pre_id,
					kind = "tool_arguments",
					delta = delta,
					offset = off,
					tool_call_id = ev.tool_call_id,
				})
			else
				local off = offsets.event
				offsets.event = off + 1
				ctx.bus:emit("item/delta", {
					turn_id = A.turn_id,
					item_id = pre_id,
					kind = "event",
					delta = "",
					offset = off,
					event = ev,
				})
			end
		end
		local function flush()
			local ev = native:take_pending()
			if ev then
				native:mark_emitted(mare.task.clock())
				emit_stream(ev)
			end
		end
		local function sink(ev)
			D.activity.delta(ev)
			local t = ev.type
			if t == "text_delta" or t == "reasoning_delta" or t == "tool_call_delta" then
				local delta = ev.delta or ""
				if not native:append(t, ev.tool_call_id, delta) then
					flush()
					-- A synchronous listener may mutate this event or install a pending
					-- event while the previous delta is dispatched. Re-read valid fields.
					delta = ev.delta or ""
					assert(
						ev.tool_call_id == nil or type(ev.tool_call_id) == "string",
						"stream event tool_call_id must be a string or nil"
					)
					assert(type(delta) == "string", "stream event delta must be a string")
					native:append_after_flush(t, ev.tool_call_id, delta)
				end
				if native:due(mare.task.clock()) then
					flush()
				end
			else
				flush()
				emit_stream(ev)
			end
		end
		local function is_message_started()
			return message_started
		end
		local function get_id()
			return pre_id
		end
		local function reset_id(status, final_msg)
			native:take_pending()
			if message_started then
				ctx.bus:emit("item/completed", {
					turn_id = A.turn_id,
					item_id = pre_id,
					status = status or "cancelled",
					item = { type = "assistant_message", message = final_msg or stream_message },
				})
			end
			pre_id = mare.crypto.random_hex(8)
			message_started = false
			offsets = { text = 0, reasoning = 0, tool_arguments = 0, event = 0 }
		end
		return sink, flush, is_message_started, get_id, reset_id
	end

	local function execute_request(S, req, request_lease, sink, flush, reset_id)
		-- request marker: a stable id, generated on the first submission
		-- and reused by network retries (llm) and crash retries (resume); written durably
		-- before the request is sent. Only the marker — never streamed partial replies.
		local rr = A.resume_request
		A.resume_request = nil
		local rid = rr and rr.request_id or mare.crypto.random_hex(12)
		local attempt0 = rr and rr.attempt or 0
		persist({ kind = "request", durable = true, payload = { request_id = rid, attempt = attempt0 } })
		req.request_id, req.attempt = rid, attempt0
		req.purpose = req.purpose or "agent" -- llm/usage: the conversation's own requests
		-- during a persistence fault neither the retry layer nor a fallback model may run
		-- on their own (they would repeat what already happened while
		-- the record is stale); llm reads `no_fallback` once the request has ended
		req.no_fallback = A.persist_error ~= nil or nil
		req.on_reset = reset_id
		req.on_attempt = function(n)
			-- no automatic retry during a persistence fault
			if A.persist_error then
				req.no_fallback = true
				return false
			end
			local ok = persist({ kind = "request", durable = true, payload = { request_id = rid, attempt = n } })
				== true
			if not ok then
				req.no_fallback = true
			end
			return ok
		end
		local off_fallback = ctx:on("llm/fallback", function() end)
		local _cleanup <close> = setmetatable({}, {
			__close = function()
				off_fallback()
			end,
		})
		D.activity.request(req)
		local msg = ctx.llm:stream(req, sink)
		D.activity.settled()
		request_lease:release()
		flush()
		msg.timestamp = mare.task.now()
		add_usage(S, msg, req.model)
		-- silent overflow detection:
		-- 1) stop with usage.input + cache_read > context_window (z.ai style)
		-- 2) length stop with output == 0 and input + cache_read >= 99% context_window (MiMo style)
		local cw = type(req.model) == "table" and tonumber(req.model.context_window) or nil
		if cw and cw > 0 and msg.stop_reason ~= "error" and type(msg.usage) == "table" then
			local input_tokens = (tonumber(msg.usage.input) or 0) + (tonumber(msg.usage.cache_read) or 0)
			local silent_stop = msg.stop_reason == "stop" and input_tokens > cw
			local silent_length = msg.stop_reason == "length"
				and (tonumber(msg.usage.output) or 0) == 0
				and input_tokens >= cw * 0.99
			if silent_stop or silent_length then
				msg.stop_reason = "error"
				msg.error_kind = "context_overflow"
				msg.error = ("silent context overflow (%s): input tokens (%d) reached context window (%d)"):format(
					silent_stop and "stop" or "length",
					input_tokens,
					cw
				)
			end
		end
		return msg, rid
	end

	local function try_recover_response(S, msg, message_started, message_id, reset_id)
		-- context overflow: drop the failed reply, run the staged
		-- compaction and retry — at most `max_overflow_compactions` rounds in
		-- a row, then stop with an error
		-- (compact injects agent, so it's looked up at call time, not declared as a dependency)
		local K = msg.stop_reason == "error" and msg.error_kind == "context_overflow" and D.follow.compact()
		if K then
			local max_rounds = cfg.max_overflow_compactions or 3
			if S.overflow_rounds < max_rounds then
				S.overflow_rounds = S.overflow_rounds + 1
				local reply = ctx.bus:waterfall("compact/overflow", {
					cancel = function()
						return S.token
					end,
				})
				local called = type(reply) == "table" and reply.succeeded == true
				local res = type(reply) == "table" and reply.res or nil
				if called and type(res) == "table" and res.status == "compacted" then
					S.turns = S.turns - 1
					return "retry"
				end
			else
				msg.error = ("the context still exceeds the model's window after %d compactions in a row (%s)"):format(
					max_rounds,
					tostring(msg.error)
				)
			end
		elseif msg.stop_reason ~= "error" then
			S.overflow_rounds = 0
			S.image_rounds = 0
		end
		-- too many / too large images: drop the failed reply, offload images to
		-- files (the older ones first, then every one) and send again; nothing left to offload
		-- → the error stands
		local KI = msg.stop_reason == "error"
			and (msg.error_kind == "image_limit" or msg.error_kind == "payload_too_large")
			and S.image_rounds < 2
			and D.follow.compact()
		if KI and KI.offload_images ~= nil and not require("mare_lib").callable(KI.offload_images) then
			ctx.bus:emit(
				"host/notice",
				{ level = "warning", text = "compact.offload_images must be a function when provided" }
			)
		elseif KI and KI.offload_images then
			local n = 0
			while S.image_rounds < 2 and n == 0 do
				S.image_rounds = S.image_rounds + 1
				n = KI:offload_images({ keep = S.image_rounds > 1 and 0 or nil }) or 0
			end
			if n > 0 then
				ctx.bus:emit("host/notice", {
					level = "info",
					text = ("the provider refused the images (%s); %d moved to files, sending again"):format(
						tostring(msg.error),
						n
					),
				})
				S.turns = S.turns - 1
				return "retry"
			end
		end
		return "proceed"
	end

	local function scan_message_call_ids(used, m)
		if type(m) ~= "table" then
			return
		end
		if m.role == "assistant" then
			for _, b in ipairs(m.content or {}) do
				if
					type(b) == "table"
					and b.type == "tool_call"
					and not b.hosted
					and type(b.id) == "string"
					and b.id ~= ""
				then
					used[b.id] = true
				end
			end
		elseif m.role == "tool_result" and type(m.tool_call_id) == "string" and m.tool_call_id ~= "" then
			used[m.tool_call_id] = true
		end
	end

	local function scan_entry_call_ids(used, e)
		if type(e) ~= "table" then
			return
		end
		if e.type == "tool_state" and type(e.tool_call_id) == "string" and e.tool_call_id ~= "" then
			used[e.tool_call_id] = true
		elseif e.type == "message" then
			scan_message_call_ids(used, e.message)
		end
	end

	local function collect_session_call_ids()
		local used = {}
		for _, m in ipairs(A.messages or {}) do
			scan_message_call_ids(used, m)
		end
		for cid in pairs(A.tool_states or {}) do
			if type(cid) == "string" and cid ~= "" then
				used[cid] = true
			end
		end
		local session = D.follow.session and D.follow.session()
		if session and session.entries then
			local ok, entries = pcall(session.entries, session, { expand = false })
			if ok and type(entries) == "table" then
				local ok_info, info = pcall(function()
					return session.info and session:info() or nil
				end)
				local sid = ok_info and type(info) == "table" and info.session_id or nil
				local epoch = ok_info and type(info) == "table" and info.session_epoch or nil
				local cache = A.entry_call_ids
				if
					not cache
					or cache.session_id ~= sid
					or cache.session_epoch ~= epoch
					or not rawequal(cache.tbl, entries)
					or #entries < cache.len
				then
					cache = { session_id = sid, session_epoch = epoch, tbl = entries, len = 0, ids = {} }
					A.entry_call_ids = cache
				end
				for i = cache.len + 1, #entries do
					scan_entry_call_ids(cache.ids, entries[i])
				end
				cache.len = #entries
				for cid in pairs(cache.ids) do
					used[cid] = true
				end
			end
		end
		return used
	end

	local function normalize_tool_call_ids(msg)
		if type(msg) ~= "table" or msg.role ~= "assistant" or type(msg.content) ~= "table" then
			return
		end
		local has_call = false
		for _, b in ipairs(msg.content) do
			if type(b) == "table" and b.type == "tool_call" and not b.hosted then
				has_call = true
				break
			end
		end
		if not has_call then
			return
		end
		local used = collect_session_call_ids()
		for _, b in ipairs(msg.content) do
			if type(b) == "table" and b.type == "tool_call" and not b.hosted then
				local raw_id = b.id
				if type(raw_id) ~= "string" or raw_id == "" or used[raw_id] then
					if raw_id ~= nil and raw_id ~= "" and b.provider_call_id == nil then
						b.provider_call_id = raw_id
					end
					local cand
					repeat
						cand = "call_" .. mare.crypto.random_hex(8)
					until not used[cand]
					b.id = cand
				end
				used[b.id] = true
			end
		end
	end

	local function handle_response_outcome(S, msg, rid, message_started, message_id, reset_id)
		-- an interrupted/failed response keeps what was streamed (marked interrupted);
		-- one with nothing in it is not added to the history at all. Unexecuted tool_call
		-- blocks from an aborted or failed model response are stripped before persistence.
		if msg.stop_reason == "aborted" or msg.stop_reason == "error" then
			local kept = {}
			for _, b in ipairs(msg.content or {}) do
				if type(b) ~= "table" or b.type ~= "tool_call" or b.hosted then
					kept[#kept + 1] = b
				end
			end
			msg.content = mare.json.array(kept)
		end
		local empty = #(msg.content or {}) == 0
		if not (empty and (msg.stop_reason == "error" or msg.stop_reason == "aborted")) then
			normalize_tool_call_ids(msg)
			append(msg, { message_started = message_started, id = message_id })
		else
			if message_started and reset_id then
				reset_id(msg.stop_reason == "aborted" and "cancelled" or "failed", msg)
			end
			-- the request ended without a reply message: not a crash on the next resume
			persist({ kind = "request-end", payload = { request_id = rid } })
		end
		local truncated = msg.stop_reason == "length"
		if truncated then
			-- Warn visibly instead of silently stopping.
			local note = "Response truncated (max output tokens reached)."
			ctx.bus:emit("host/notice", { level = "warning", text = note })
			if mare.ui.connected() then
				mare.ui.notify(note, "warning")
			end
		end
		if msg.stop_reason == "error" then
			S.run_error = msg.error or "model error"
			S.error_kind = msg.error_kind
			S.termination_reason = outcome.model_error(S.error_kind)
			return "break", truncated
		end
		if msg.stop_reason == "aborted" or S.token:is_cancelled() then
			S.interrupt()
			return "break", truncated
		end
		if msg.stop_reason == "paused" then
			-- Anthropic pause_turn: send
			-- the turn back as it is to let the server continue — no user message is added
			return "continue", truncated
		end
		if msg.stop_reason == "filtered" then
			-- refused / content-filtered: say so, never retried
			local note = tostring(msg.error or "the model declined to answer")
			ctx.bus:emit("host/notice", { level = "warning", text = note })
			if mare.ui.connected() then
				mare.ui.notify(note, "warning")
			end
		end
		local text = ctx.llm:text_of(msg)
		S.final_text = text
		if text ~= "" then
			S.last_text = text
		end
		return "ok", truncated
	end

	return {
		build_request = build_request,
		fail_step = fail_step,
		create_stream_sink = create_stream_sink,
		execute_request = execute_request,
		try_recover_response = try_recover_response,
		handle_response_outcome = handle_response_outcome,
	}
end
