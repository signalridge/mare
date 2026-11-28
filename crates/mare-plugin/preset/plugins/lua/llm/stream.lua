-- llm.stream: streaming with retries, the crash guard, model fallback, remote compaction;
-- part of the `llm` plugin (plugins/llm.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local providers, lib, llm = D.providers, D.lib, D.llm
	local IDLE_MULT = D.IDLE_MULT
	local params = D.params
	local classify = D.errors.classify
	local transform_messages = D.history.transform_messages
	local side_calls = D.side_calls or {}
	local projection = require("llm.projection")

	--- Track auxiliary work until its result and usage event have both been handled.
	--- The token is a child of the caller's token, so either owner can cancel it.
	local function with_side_request(req, run)
		local failure = mare.task.session_error()
		if failure then
			error(failure, 0)
		end
		if (req.purpose or "other") == "agent" then
			return run(req)
		end
		local copy = {}
		for k, v in pairs(req) do
			copy[k] = v
		end
		local session = D.follow.session()
		local info = session and session.info and session:info() or nil
		copy._usage_owner = info and info.session_id or nil
		copy._usage_at = info and info.leaf_entry_id or nil
		copy.cancel = req.cancel and req.cancel:child() or mare.task.token()
		local entry = { owner = copy._usage_owner, token = copy.cancel }
		side_calls[entry] = true
		local side_guard <close> = setmetatable({}, {
			__close = function()
				side_calls[entry] = nil
			end,
		})
		local result = table.pack(pcall(run, copy))
		if not result[1] then
			error(result[2], 0)
		end
		return table.unpack(result, 2, result.n)
	end

	function llm.cancel_side_requests(_, _, owner)
		for entry in pairs(side_calls) do
			if entry.owner == owner then
				entry.token:cancel()
			end
		end
	end

	function llm.side_requests_pending(_, _, owner)
		for entry in pairs(side_calls) do
			if entry.owner == owner then
				return true
			end
		end
		return false
	end

	ctx:effect(function()
		return function()
			for entry in pairs(side_calls) do
				entry.token:cancel()
			end
		end
	end, "llm side calls")

	local function err_text(err)
		return D.errors.summarize_error(err)
	end

	local function notify_reset(reset_path)
		if not reset_path then
			return
		end
		local note = ("crash guard state was corrupt and reset (backup: %s); crash counts were cleared"):format(
			tostring(reset_path)
		)
		mare.log("warn", note)
		if mare.ui and mare.ui.notify then
			mare.ui.notify(note, "warning")
		end
	end

	-- crash-guard marks this process holds (fingerprint -> requests in flight). A clean close
	-- (dispose: exit, /reload) ends them with `release`: the turn was cut off, mare didn't
	-- crash, so the next `begin` of the same request must not count one (
	-- the count of earlier real crashes is kept)
	local inflight = {}
	ctx:effect(function()
		return function()
			local fps = inflight
			inflight = {}
			for fp in pairs(fps) do
				local ok, res = pcall(mare.crash.release, fp)
				if not ok then
					mare.log("warn", "crash guard: " .. tostring(res))
				else
					notify_reset(res)
				end
			end
		end
	end, "llm crash marks")

	--- fingerprint(req) -> the crash-guard key of a request: the stable
	--- request id when the caller has one (the agent generates it on the first submission and
	--- reuses it across network and crash retries, so the count accumulates across a resume);
	--- otherwise (one-off requests, e.g. summaries) session, model and the tail of the history.
	function llm.fingerprint(_, _, req)
		if type(req.request_id) == "string" and req.request_id ~= "" then
			return "req:" .. req.request_id
		end
		local msgs = req.messages or {}
		local tail = {}
		for i = math.max(1, #msgs - 3), #msgs do
			tail[#tail + 1] = msgs[i]
		end
		local ok, enc = pcall(mare.json.encode, tail)
		local model = req.model or {}
		return mare.crypto.sha256(table.concat({
			tostring(req.session_id or ""),
			tostring(model.ref or model.id or ""),
			tostring(#msgs),
			ok and enc or "",
		}, "\0"))
	end

	--- Stream a completion with retries (only before any content arrived).
	function llm.stream(self, caller, req, sink)
		local mare_code = D.errors.mare_code
		local model = req.model or self:current(caller)
		local readiness = require("llm.readiness")
		if not model then
			lib.fail("llm.no_model", readiness.no_model())
		end
		if model.provider then
			if D.catalog.ensure_loaded(model.provider) then
				local hydrated = model.ref and self:resolve(caller, model.ref) or nil
				if hydrated then
					model = hydrated
				end
			end
		end
		local p = providers[model.provider]
		if not p then
			error("provider not available: " .. tostring(model.provider))
		end
		-- per-provider retries and timeouts (mare.provider{ retries, stream_idle_timeout_ms })
		local pcfg = D.api_keys.provider_cfg(model.provider)
		local retries = tonumber(pcfg.retries) or cfg.retries
		-- missing credentials: say how to provide them for this frontend (headless: `mare login`
		-- or the variable, not /login) or the provider's `login_hint`; a key
		-- command is always tried. The agent's prompts are refused earlier, by request/ready.
		local missing = readiness.missing_credentials(p, pcfg, function(name)
			return D.how_to_login(name)
		end)
		if missing then
			return {
				role = "assistant",
				content = mare.json.array(),
				stop_reason = "error",
				error = missing,
				error_kind = "auth",
				error_code = mare_code("auth"),
				error_data = { kind = "auth", error_code = mare_code("auth"), provider = model.provider },
				model_id = model.id,
				provider_id = model.provider,
			}
		end
		local gateway, source, wire_route, route_error, auth, resolved = D.routes.route_of(model)
		if route_error then
			return {
				role = "assistant",
				content = mare.json.array(),
				stop_reason = "error",
				error = route_error,
				error_kind = "invalid",
				error_code = mare_code("invalid"),
				error_data = { kind = "invalid", error_code = mare_code("invalid"), provider = model.provider },
				model_id = model.id,
				provider_id = model.provider,
			}
		end
		-- the provider sees a converted copy; the caller's (session's) history is untouched
		do
			local copy = {}
			for k, v in pairs(req) do
				copy[k] = v
			end
			-- a provider with several routes: thinking from another route of the same model is not
			-- replayed (a message without one went over the provider's default route)
			local route = p.routes and gateway or nil
			copy.routes = {
				gateway = gateway,
				source = source,
				wire = wire_route,
				auth = auth,
				config = resolved and resolved.config,
				describe = resolved and resolved.describe,
				stream = resolved and resolved.stream,
			}
			copy.messages =
				transform_messages(req.messages or {}, model, req.tools, p.tool_id_policy, route, p.route, wire_route)
			-- every request of a session carries its id, auxiliary ones (summaries, titles)
			-- included: providers key per-session headers on it (e.g. `x-opencode-session`)
			if copy.session_id == nil then
				local S = D.follow.session()
				local info = S and S.info and S:info()
				copy.session_id = info and info.session_id or nil
			end
			req = copy
		end
		if req.output_schema ~= nil then
			local ok_schema, compiled = pcall(mare.schema.compile_output, req.output_schema)
			if not ok_schema then
				return {
					role = "assistant",
					content = mare.json.array(),
					stop_reason = "error",
					error = "invalid output_schema: " .. tostring(compiled),
					error_kind = "invalid",
					error_code = mare_code("invalid"),
					error_data = { kind = "invalid", error_code = mare_code("invalid"), provider = model.provider },
					model_id = model.id,
					provider_id = model.provider,
				}
			end
			req.output_schema = compiled:value()
		end
		req.model = model
		-- the model's parameters by layer (llm/params): the effort the session / config / model
		-- default asks for, adjusted to what the model offers (never silently: llm/params-adjusted
		-- and /profile); an explicit req.effort is sent as it is
		local overlay = req.params_overlay
		if overlay and overlay.route ~= wire_route then
			return {
				role = "assistant",
				content = mare.json.array(),
				stop_reason = "error",
				error = "the provider route changed after request parameters were prepared",
				error_kind = "invalid",
				error_code = mare_code("invalid"),
				error_data = { kind = "invalid", error_code = mare_code("invalid"), provider = model.provider },
				model_id = model.id,
				provider_id = model.provider,
			}
		end
		local selected, selection_error = params.resolve_request(req, wire_route, overlay)
		if not selected then
			return {
				role = "assistant",
				content = mare.json.array(),
				stop_reason = "error",
				error = selection_error,
				error_kind = "invalid",
				error_code = mare_code("invalid"),
				error_data = { kind = "invalid", error_code = mare_code("invalid"), provider = model.provider },
				model_id = model.id,
				provider_id = model.provider,
			}
		end
		-- text streamed so far in the current attempt: an aborted / failed response keeps it,
		-- marked interrupted. Thinking and partial tool calls are not
		-- kept (unsigned thinking / cut-off arguments can't be replayed).
		local partial = {}
		local function partial_content()
			local out = mare.json.array()
			for _, b in ipairs(partial) do
				local text = b.buffer and b.buffer:snapshot() or b.text
				if text ~= "" then
					out[#out + 1] = { type = "text", text = text }
				end
			end
			return out
		end
		local ALLOWED_ERROR_DATA_SCALARS = {
			raw_code = true,
			raw_type = true,
			status = true,
			provider = true,
			request_id = true,
			source = true,
		}
		local function is_scalar(v)
			local tv = type(v)
			return tv == "string" or tv == "number" or tv == "boolean"
		end
		local VALID_ERROR_KINDS = {
			auth = true,
			quota = true,
			usage_limit = true,
			rate_limit = true,
			overloaded = true,
			server = true,
			transport = true,
			stream_closed = true,
			context_overflow = true,
			image_limit = true,
			payload_too_large = true,
			malformed_response = true,
			model_not_found = true,
			refusal = true,
			invalid = true,
			fatal = true,
			retry_delay_too_long = true,
			aborted = true,
			crash_limit = true,
			persist = true,
		}
		local function safe_error_kind(v)
			local canonical = D.errors.canonical_kind(v)
			return canonical and VALID_ERROR_KINDS[canonical] and canonical or nil
		end
		local function failed(res, kind, original_kind)
			local content = partial_content()
			local t = type(res) == "table" and res or nil
			local err_kind = safe_error_kind(kind) or safe_error_kind(t and t.kind) or "fatal"
			local err_code = mare_code(err_kind)
			local orig_kind = safe_error_kind(original_kind) or safe_error_kind(t and t.original_kind)
			local err_data = {}
			if t and type(t.data) == "table" then
				for k, v in pairs(t.data) do
					if ALLOWED_ERROR_DATA_SCALARS[k] and is_scalar(v) then
						err_data[k] = v
					end
				end
			end
			err_data.kind = err_kind
			err_data.error_code = err_code
			if orig_kind ~= nil then
				err_data.original_error_kind = orig_kind
			end
			if t then
				if err_data.raw_code == nil and is_scalar(t.raw_code) then
					err_data.raw_code = t.raw_code
				elseif err_data.raw_code == nil then
					local vc = t.api_code ~= nil and t.api_code or t.code
					if vc ~= nil and is_scalar(vc) and not (type(vc) == "string" and vc:match("^llm%.")) then
						err_data.raw_code = tostring(vc)
					end
				end
				if err_data.raw_type == nil and is_scalar(t.raw_type) then
					err_data.raw_type = t.raw_type
				elseif err_data.raw_type == nil then
					local vt = t.api_type ~= nil and t.api_type or t.type
					if vt ~= nil and is_scalar(vt) then
						err_data.raw_type = tostring(vt)
					end
				end
				if err_data.status == nil and is_scalar(t.status) then
					err_data.status = t.status
				end
				if err_data.request_id == nil and is_scalar(t.request_id) then
					err_data.request_id = t.request_id
				end
				if err_data.source == nil and is_scalar(t.source) then
					err_data.source = t.source
				end
			end
			err_data.provider = model.provider
			return {
				role = "assistant",
				content = content,
				interrupted = #content > 0 or nil,
				stop_reason = "error",
				error = err_text(res),
				error_kind = err_kind,
				error_code = err_code,
				error_data = next(err_data) and err_data or nil,
				original_error_kind = orig_kind,
				usage = t and t.usage or nil,
				model_id = model.id,
				provider_id = model.provider,
			}
		end
		local function aborted()
			local content = partial_content()
			return {
				role = "assistant",
				content = content,
				interrupted = #content > 0 or nil,
				stop_reason = "aborted",
				model_id = model.id,
				provider_id = model.provider,
			}
		end
		-- idle watchdog, header timeout, and default first-event deadline for every provider
		-- (keep explicit req.first_event_timeout_ms untouched so send.lua resolves
		-- request -> endpoint -> computed default in one place)
		local idle = (tonumber(pcfg.stream_idle_timeout_ms) or cfg.stream_idle_timeout_ms or 300000)
			* (IDLE_MULT[req.effort or ""] or 1)
		req.idle_timeout_ms = req.idle_timeout_ms or idle
		req.header_timeout_ms = req.header_timeout_ms or idle
		req._default_first_event_timeout_ms = idle
		req.sse_max_line_bytes = req.sse_max_line_bytes or cfg.sse_max_line_bytes
		req.sse_max_event_bytes = req.sse_max_event_bytes or cfg.sse_max_event_bytes
		local function emit_usage(result)
			if type(result) == "table" and type(result.usage) == "table" then
				ctx.bus:emit("llm/usage", {
					usage = result.usage,
					model_ref = model.ref,
					provider_id = model.provider,
					subscription = result.subscription,
					purpose = req.purpose or "other",
					request_id = req.request_id,
					session_id = req._usage_owner,
					leaf_entry_id = req._usage_at,
				})
			end
		end
		-- crash guard: durably mark this request in flight; a mark a
		-- dead process left behind is one crash. Refuse to resend after crash_limit crashes.
		local fp = llm.fingerprint(nil, nil, req)
		local inflight_added = false
		local guarded = false
		local crash_settled = false
		local function ended()
			if not inflight_added then
				return
			end
			inflight_added = false
			local n = (inflight[fp] or 1) - 1
			inflight[fp] = n > 0 and n or nil
		end
		-- Install `<close>` before `begin` yields so a coroutine abort mid-`begin` also cleans up
		-- `inflight[fp]` (while Rust's `BeginCancelGuard` releases the on-disk mark).
		local _crash_guard <close> = lib.guard(function()
			if guarded and not crash_settled then
				crash_settled = true
				local okr, rres = pcall(mare.crash.release, fp)
				if not okr then
					mare.log("warn", "crash guard: " .. tostring(rres))
				else
					notify_reset(rres)
				end
			end
			ended()
		end)
		-- Register before `begin` yields so a clean shutdown arriving mid-`begin` still releases `fp`.
		inflight[fp] = (inflight[fp] or 0) + 1
		inflight_added = true
		-- a state directory that can't be written degrades the guard, not every request
		local ok_begin, crashes, reset_path = pcall(mare.crash.begin, fp)
		guarded = ok_begin
		if not guarded then
			ended()
			mare.log("warn", "crash guard unavailable, request sent unguarded: " .. tostring(crashes))
			crashes = 0
		else
			notify_reset(reset_path)
		end
		if crashes >= (cfg.crash_limit or 3) then
			local res = failed({
				message = (
					"this request crashed mare %d times in a row; it is not sent again automatically — "
					.. "change the last message, /model or /compact, then retry"
				):format(crashes),
				kind = "crash_limit",
			}, "crash_limit")
			ctx.bus:emit("llm/crash-limit", { crashes = crashes, fingerprint = fp })
			-- not sent: `_crash_guard` releases the mark (evictable, no extra crash next time) while keeping the count
			return res
		end
		-- network retries and crash retries share one attempt count (
		-- durable): a resumed request starts from the attempts it already used
		local attempt, overloaded = math.max(0, math.floor(tonumber(req.attempt) or 0)), 0
		local auth_refreshed = false
		local reply_capped = false
		local function before_retry(next_attempt_num, delay_ms, err_res, kind, reset, max_attempts)
			-- the caller records the new attempt (request marker) before it is sent
			if req.on_attempt then
				local okr, perr = pcall(req.on_attempt, next_attempt_num)
				if not okr then
					mare.log("warn", "on_attempt failed before a retry: " .. err_text(perr))
				end
				if not okr or perr == false then
					return failed({ message = "session write failed before a retry", kind = "persist" }, "persist")
				end
			end
			local ev_err = {
				code = (
					type(err_res) == "table"
					and type(err_res.code) == "string"
					and err_res.code ~= ""
					and err_res.code
				)
					or (type(kind) == "string" and kind ~= "" and kind)
					or "error",
				message = err_text(err_res),
			}
			if type(err_res) == "table" and err_res.data ~= nil then
				ev_err.data = err_res.data
			end
			ctx.bus:emit("llm/retry", {
				attempt = next_attempt_num,
				max_attempts = max_attempts,
				delay_ms = delay_ms,
				error = ev_err,
				kind = kind,
				reset = reset == true,
			})
			if req.turn_id and req.step_id then
				ctx.bus:emit("step/completed", {
					turn_id = req.turn_id,
					step_id = req.step_id,
					status = "failed",
					error = ev_err,
					error_kind = kind,
					will_retry = true,
					attempt = next_attempt_num,
					max_attempts = max_attempts,
					delay_ms = delay_ms,
					reset = reset,
				})
				if reset and req.on_reset then
					pcall(req.on_reset, "failed")
				end
				ctx.bus:emit("step/started", {
					turn_id = req.turn_id,
					step_id = req.step_id,
					turn = req.turn,
					model = req.model,
					attempt = next_attempt_num,
				})
			end
			return nil
		end
		local function attempts()
			while true do
				-- nothing is committed before the provider's end signal: a failed attempt's partial
				-- text and tool-call deltas are discarded and the UI is reset
				local emitted = false
				partial = {}
				local filtered = projection.sink()
				local function publish(ev)
					if ev.type == "text_delta" then
						local last = partial[#partial]
						if not last or last.tool_call_id ~= ev.tool_call_id then
							last = { tool_call_id = ev.tool_call_id, buffer = mare.stream.buffer() }
							partial[#partial + 1] = last
						end
						local delta = ev.delta or ""
						if last.buffer and type(delta) == "string" then
							last.buffer:append(delta)
						else
							-- Keep Lua concatenation behavior for an out-of-contract provider value.
							if last.buffer then
								last.text, last.buffer = last.buffer:snapshot(), nil
							end
							last.text = last.text .. delta
						end
					end
					if sink then
						sink(ev)
					end
				end
				local function wrapped(ev)
					emitted = true
					local safe = filtered:push(ev)
					if safe then
						publish(safe)
					end
				end
				local ok, res = pcall(
					p.stream,
					-- a request already constrained (and bound) keeps that one snapshot on every
					-- attempt; anything else (side requests, no executable binding) is constrained here
					projection.request(req._tools_constrained and req or D.constrain_tools(req)),
					wrapped
				)
				local flushed, flush_error = pcall(function()
					if ok and type(res) == "table" and res.stop_reason ~= "aborted" and res.stop_reason ~= "error" then
						for _, ev in ipairs(filtered:finish()) do
							publish(ev)
						end
					else
						filtered:close()
					end
				end)
				if not flushed then
					ok, res = false, flush_error
				end

				res = mare.projection.project(res)
				-- Preserve usage already returned by a side attempt, even if its empty
				-- response is subsequently retried or another provider is tried.
				if (req.purpose or "other") ~= "agent" then
					emit_usage(res)
				end
				local has_visible = false
				if ok and res then
					for _, b in ipairs(res.content or {}) do
						if type(b) == "table" then
							if b.type == "text" then
								if type(b.text) == "string" and b.text:match("%S") ~= nil then
									has_visible = true
									break
								end
							elseif b.type ~= "reasoning" then
								has_visible = true
								break
							end
						end
					end
				end
				if
					ok
					and res
					and res.stop_reason ~= "aborted"
					and res.stop_reason ~= "filtered"
					and res.stop_reason ~= "paused"
					and not has_visible
					and res.stop_reason ~= "error"
				then
					-- Never persist reasoning-only or whitespace-only text as an interrupted partial reply.
					partial = {}
					if res.stop_reason == "length" then
						local cw = type(model) == "table" and tonumber(model.context_window) or nil
						local u = type(res.usage) == "table" and res.usage or nil
						local lib = require("mare_lib")
						local inp = u and lib.usage_window_input(u) or 0
						if cw and cw > 0 and u and lib.usage_window_output(u) == 0 and inp >= cw * 0.99 then
							-- Xiaomi MiMo style silent overflow
							ok, res =
								false, {
									message = ("silent context overflow (length): input tokens (%d) reached context window (%d)"):format(inp, cw),
									kind = "context_overflow",
								}
						else
							-- thinking/reasoning consumed the entire max_tokens budget with zero visible
							-- output; retrying with the same parameters would burn the budget again
							ok, res =
								false, {
									message = "empty response from model (cut at max_tokens — thinking exhausted output budget; increase max_tokens or lower reasoning effort)",
									kind = "invalid",
									should_retry = false,
								}
						end
					else
						-- a clean end with no content is transient;
						-- an empty answer the content filter produced is not
						ok, res = false, { message = "empty response from model", kind = "server" }
					end
				end
				if ok then
					if res.stop_reason == "aborted" and (mare.projection.active() or #(res.content or {}) == 0) then
						res.content = partial_content()
						res.interrupted = #res.content > 0 or nil
					end
					res.model_id = res.model_id or model.id
					res.provider_id = model.provider
					if res.stop_reason == "error" then
						local rcode_raw = res.raw_stop_reason ~= nil and res.raw_stop_reason or res.raw_code
						local err_tbl = {
							message = (type(res.error) == "string" and res.error ~= "") and res.error or err_text(res),
							kind = safe_error_kind(res.error_kind) or "malformed_response",
							code = is_scalar(rcode_raw) and rcode_raw or nil,
							raw_code = is_scalar(rcode_raw) and rcode_raw or nil,
							raw_type = is_scalar(res.raw_type) and res.raw_type or nil,
							status = is_scalar(res.status) and res.status or nil,
							provider = model.provider,
							source = "stream",
						}
						local rkind, _ = classify(err_tbl, false)
						rkind = safe_error_kind(rkind) or "malformed_response"
						local rcode = mare_code(rkind)
						res.error = mare.projection.project(err_tbl.message)
						res.error_kind = rkind
						res.error_code = rcode
						res.original_error_kind = safe_error_kind(res.original_error_kind)
						res.error_data = {
							kind = rkind,
							error_code = rcode,
							raw_code = err_tbl.raw_code ~= nil and mare.projection.project(tostring(err_tbl.raw_code))
								or nil,
							raw_type = err_tbl.raw_type ~= nil and mare.projection.project(tostring(err_tbl.raw_type))
								or nil,
							status = err_tbl.status,
							provider = model.provider,
							source = "stream",
						}
					end
					return res
				end
				local cancelled = req.cancel and req.cancel:is_cancelled()
				-- prompt fits in the context window, only prompt + max_tokens exceeded it:
				-- cap max_tokens to the remaining window and resend once instead of compacting
				local min_reply = tonumber(type(model) == "table" and model.min_output_tokens)
				if min_reply then
					min_reply = math.max(1, math.floor(min_reply))
				else
					min_reply =
						math.min(1024, math.max(1, tonumber(type(model) == "table" and model.max_tokens) or 1024))
				end
				local current_cap = (req.params_overlay and req.params_overlay.reserved) or req.max_tokens
				if
					not cancelled
					and not reply_capped
					and attempt < retries
					and type(res) == "table"
					and type(res.reply_left) == "number"
					and res.reply_left >= min_reply
					and (current_cap == nil or current_cap > res.reply_left)
				then
					reply_capped = true
					req.max_tokens = res.reply_left
					req._reply_cap = res.reply_left
					attempt = attempt + 1
					local persist_err = before_retry(attempt, 0, res, "context_overflow", emitted, retries)
					if persist_err then
						return persist_err
					end
					goto next_attempt
				end
				local kind, retryable
				if type(res) == "table" and (not res._classified or res.source ~= nil or type(p.errors) == "table") then
					local _, k, r =
						D.errors.classify_raw(res, { provider = model.provider, cancelled = cancelled }, providers)
					kind, retryable = k, r
				elseif type(res) == "table" and res._classified and not cancelled then
					kind = D.errors.canonical_kind(res.kind) or "fatal"
					retryable = res.retryable == true
					local code = mare_code(kind)
					res.kind, res.retryable, res.error_code = kind, retryable, code
					if type(res.data) == "table" then
						res.data.kind = kind
						res.data.error_code = code
					end
				else
					kind, retryable = classify(res, cancelled)
					if type(res) == "table" then
						local code = mare_code(kind)
						res.kind, res.retryable, res.error_code = kind, retryable, code
						if type(res.data) == "table" then
							res.data.kind = kind
							res.data.error_code = code
						end
					end
				end
				if kind == "aborted" then
					return aborted()
				end
				-- a key/header/query helper's value was rejected: run it again once (a 401 forces a refresh)
				if kind == "auth" and not auth_refreshed and D.api_keys.has_cmd_auth(model.provider) then
					auth_refreshed = true
					llm.invalidate_api_key(llm, nil, model.provider)
					goto next_attempt
				end
				attempt = attempt + 1
				if kind == "overloaded" then
					overloaded = overloaded + 1
				end
				if
					not retryable
					or attempt > retries
					or (kind == "overloaded" and overloaded > (cfg.overloaded_retries or 3))
				then
					return failed(res, kind)
				end
				local server_ms = type(res) == "table" and res.retry_after_ms or nil
				if not server_ms and kind == "rate_limit" then
					server_ms = lib.retry_after_from_text(type(res) == "table" and res.message)
				end
				local delay
				if server_ms then
					if server_ms > (cfg.retry_after_max_ms or 60000) then
						local t = type(res) == "table" and res or nil
						return failed({
							message = ("the server asked to wait %ds before retrying (limit %ds): %s"):format(
								math.ceil(server_ms / 1000),
								math.ceil((cfg.retry_after_max_ms or 60000) / 1000),
								err_text(res)
							),
							kind = "retry_delay_too_long",
							original_kind = kind,
							raw_code = t and t.raw_code or nil,
							raw_type = t and t.raw_type or nil,
							status = t and t.status or nil,
							request_id = t and t.request_id or nil,
							source = t and t.source or nil,
							provider = (t and t.provider) or model.provider,
						}, "retry_delay_too_long", kind)
					end
					delay = server_ms
				else
					-- exponential backoff with symmetric ±10% jitter, clamped at the cap
					local cap = cfg.retry_max_ms
					local base = math.min(cfg.retry_base_ms * 2 ^ (attempt - 1), cap)
					delay = math.min(base * (0.9 + math.random() * 0.2), cap)
				end
				-- a transport failure retries on a new connection, not a pooled one
				if kind == "transport" or kind == "stream_closed" then
					req.fresh_connection = true
				end
				local max_attempts = kind == "overloaded"
						and math.min(retries, attempt - overloaded + (cfg.overloaded_retries or 3))
					or retries
				local persist_err = before_retry(attempt, delay, res, kind, emitted, max_attempts)
				if persist_err then
					return persist_err
				end
				if not mare.task.sleep(delay, req.cancel) then
					return aborted()
				end
				::next_attempt::
			end
		end
		local ok, res = pcall(attempts)
		if guarded and not crash_settled then
			crash_settled = true
			_crash_guard:disarm()
			-- the mark stays ours until `finish` is on disk: a close while it is still being
			-- written releases it (dispose above)
			local okf, fres = pcall(mare.crash.finish, fp)
			if not okf then
				mare.log("warn", "crash guard: " .. tostring(fres))
			else
				notify_reset(fres)
			end
			ended()
		end
		if not ok then
			error(res, 0)
		end
		-- per-request token usage for usage / statusline plugins (each fallback attempt
		-- reports its own); the conversation's requests carry purpose "agent"
		if (req.purpose or "other") == "agent" then
			emit_usage(res)
		end
		return res
	end

	--- fallback_models: when the request failed
	--- because the model is overloaded, doesn't exist or its quota is used up — after its own
	--- retries — the next of up to 3 fallbacks answers this request. The answer records the
	--- model that produced it (`model` / `provider`) and `fallback_from`; `llm/fallback` and a
	--- notification say what happened. The current model stays as it is.
	local function fallback_worthy(res)
		if type(res) ~= "table" or res.stop_reason ~= "error" then
			return false
		end
		local k = res.error_kind
		-- a retryable error whose Retry-After exceeded the limit keeps its own kind for this
		-- decision: an overloaded model asking for a long wait is still overloaded
		if k == "retry_delay_too_long" and res.original_error_kind then
			k = res.original_error_kind
		end
		if
			k == "overloaded"
			or k == "quota"
			or k == "usage_limit"
			or k == "model_not_found"
			or k == "rate_limit"
			or k == "server"
			or k == "transport"
			or k == "stream_closed"
		then
			return true
		end
		local e = string.lower(tostring(res.error or ""))
		return (k == "invalid" or k == "auth" or k == "fatal" or k == nil)
			and (
				e:find("model_not_found", 1, true) ~= nil
				or e:find("model not found", 1, true) ~= nil
				or (e:find("model", 1, true) ~= nil and e:find("does not exist", 1, true) ~= nil)
				or e:find("unknown model", 1, true) ~= nil
				or e:find(" 404:", 1, true) ~= nil
				or e:find("token plan", 1, true) ~= nil
				or e:find("in your plan", 1, true) ~= nil
				or e:find("current plan", 1, true) ~= nil
				or e:find("model_unserved", 1, true) ~= nil
				or (
					e:find("model", 1, true) ~= nil
					and (e:find("not subscribed", 1, true) ~= nil or e:find("not available in", 1, true) ~= nil)
				)
			)
	end
	local stream_one = llm.stream
	local function stream_with_fallback(self, caller, req, sink)
		local emitted = false
		local function tracked_sink(ev)
			if
				type(ev) == "table"
				and (
					ev.type == "text_delta"
					or ev.type == "reasoning_delta"
					or ev.type == "tool_call_start"
					or ev.type == "tool_call_delta"
				)
			then
				emitted = true
			end
			if sink then
				sink(ev)
			end
		end
		local res = stream_one(self, caller, req, tracked_sink)
		if req.no_fallback and not (req.cancel and req.cancel:is_cancelled()) and fallback_worthy(res) then
			-- the caller opted out of `fallback_models` to run its own chain: tell it this error
			-- is one they would have answered (overloaded, missing model, quota)
			res.fallback_worthy = true
		end
		local fb = mare.config.fallback_models
		if
			(req.cancel and req.cancel:is_cancelled())
			or type(fb) ~= "table"
			or #fb == 0
			or req.no_fallback
			or not fallback_worthy(res)
		then
			return res
		end
		local first = req.model or llm.current(llm, caller)
		local tried = { [first and first.ref or ""] = true }
		for i = 1, math.min(#fb, 3) do
			if req.cancel and req.cancel:is_cancelled() then
				break
			end
			local m = llm.resolve(llm, caller, fb[i])
			if m and not tried[m.ref] then
				tried[m.ref] = true
				local why = res.error_kind or "error"
				ctx.bus:emit(
					"llm/fallback",
					{ from = first and first.ref, to = m.ref, reason = why, error = res.error }
				)
				local note = ("%s failed (%s) — answering with %s"):format(first and first.ref or "?", why, m.ref)
				mare.log("warn", note)
				if mare.ui.connected() then
					mare.ui.notify(note, "warning")
				end
				local r2 = {}
				for k, v in pairs(req) do
					r2[k] = v
				end
				r2.model = m
				if r2._rebuild_tool_declarations then
					r2 = r2._rebuild_tool_declarations(r2)
				elseif r2.effort ~= nil then
					-- Direct llm:stream callers have no agent rebuild to clamp the fallback.
					r2.effort = llm.clamp_effort(llm, caller, m, r2.effort)
				end
				if emitted then
					local ev_err = {
						code = (type(res.error_code) == "string" and res.error_code ~= "" and res.error_code)
							or (type(why) == "string" and why ~= "" and why)
							or "error",
						message = tostring(res.error or ""),
					}
					if res.error_data ~= nil then
						ev_err.data = res.error_data
					end
					ctx.bus:emit("llm/retry", {
						attempt = 1,
						delay_ms = 0,
						error = ev_err,
						kind = why,
						reset = true,
					})
					if req.turn_id and req.step_id then
						ctx.bus:emit("step/completed", {
							turn_id = req.turn_id,
							step_id = req.step_id,
							status = "failed",
							error = ev_err,
							error_kind = why,
							will_retry = true,
							attempt = 1,
							delay_ms = 0,
							reset = true,
						})
						if req.on_reset then
							pcall(req.on_reset, "failed")
						end
						ctx.bus:emit("step/started", {
							turn_id = req.turn_id,
							step_id = req.step_id,
							turn = req.turn,
							model = r2.model,
							attempt = 1,
						})
					elseif req.on_reset then
						pcall(req.on_reset, "failed")
					end
					emitted = false
				end
				local res2 = stream_one(self, caller, r2, tracked_sink)
				res2.fallback_from = first and first.ref
				res2.fallback_reason = why
				if not fallback_worthy(res2) then
					return res2
				end
				res = res2
			end
		end
		return res
	end

	function llm.stream(self, caller, req, sink)
		return projection.call(with_side_request, req, function(owned)
			return stream_with_fallback(self, caller, owned, sink)
		end)
	end

	--- supports_remote_compact(model?) → whether compact_remote can work for the model: its
	--- provider has a compaction endpoint and the model doesn't opt out (remote_compact = false).
	--- The compact plugin checks this before preparing a remote request (offloading images).
	function llm.supports_remote_compact(self, caller, model)
		model = model or self:current(caller)
		local p = model and providers[model.provider]
		return (p and p.compact and model.remote_compact ~= false) and true or false
	end

	--- Provider-side compaction. Returns
	--- {items, model, provider} | nil, err ("unsupported" when the provider has none).
	local function compact_remote(self, caller, req)
		local model = req.model or self:current(caller)
		if not self:supports_remote_compact(caller, model) then
			return nil, "unsupported", "unsupported"
		end
		local p = providers[model.provider]
		req.model = model
		if req.effort == nil then
			req.effort = self:clamp_effort(caller, model)
		end
		local request_params = {}
		for k, v in pairs(req.params or {}) do
			request_params[k] = v
		end
		request_params.effort = request_params.effort or req.effort
		req.params = request_params
		local ok, res, err, kind, failed_usage, failed_subscription = pcall(p.compact, projection.request(req))
		res, err = mare.projection.project(res), mare.projection.project(err)
		local function emit_compact_usage(usage, subscription)
			if type(usage) == "table" then
				ctx.bus:emit("llm/usage", {
					usage = usage,
					model_ref = model.ref,
					provider_id = model.provider,
					subscription = subscription,
					purpose = req.purpose or "compact",
					session_id = req._usage_owner,
					leaf_entry_id = req._usage_at,
				})
			end
		end
		if not ok then
			if type(res) == "table" then
				emit_compact_usage(res.usage, res.subscription)
			end
			return nil, err_text(res), "server"
		end
		if not res then
			emit_compact_usage(failed_usage, failed_subscription)
			-- kind: "cancelled" / "policy" stop compaction; anything else falls back to local
			return nil, err or "remote compaction failed", kind or "server"
		end
		res.provider_id = model.provider
		res.model_id = res.model_id or res.model or model.id
		res.provider, res.model = nil, nil
		emit_compact_usage(res.usage, res.subscription)
		return res
	end

	function llm.compact_remote(self, caller, req)
		return projection.call(with_side_request, req, function(owned)
			return compact_remote(self, caller, owned)
		end)
	end

	--- Non-streaming convenience: returns assistant message.
	function llm.complete(self, caller, req)
		return self:stream(caller, req, nil)
	end

	--- Plain text of an assistant message (including fallback text on dotted extension blocks).
	function llm.text_of(_, _, msg)
		local out = {}
		for _, b in ipairs(msg and msg.content or {}) do
			if type(b) == "table" and type(b.text) == "string" then
				if b.type == "text" or (type(b.type) == "string" and b.type:find(".", 1, true) ~= nil) then
					out[#out + 1] = b.text
				end
			end
		end
		return table.concat(out)
	end
end
