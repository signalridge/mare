-- compact.stages.summarize: message text helpers, summary requests, protocol units and the
-- budgeted summarize loop; part of the lazy `compact.stages` module (lua/compact/stages.lua),
-- loaded at the start of its factory, i.e. on the first compaction.
return function(D)
	local ctx, cfg, S = D.ctx, D.cfg, D.S
	local tokens, msg_tokens, estimate, budgets = D.tokens, D.msg_tokens, D.estimate, D.budgets
	local SYSTEM = D.prompts.SYSTEM

	local function text_of(m)
		local parts = {}
		for _, b in ipairs(m.content or {}) do
			if type(b) == "table" and type(b.text) == "string" then
				if b.type == "text" or (type(b.type) == "string" and b.type:find(".", 1, true) ~= nil) then
					parts[#parts + 1] = b.text
				end
			end
		end
		return table.concat(parts, "\n")
	end
	--- first / last `n` code points of `s` (never splits a UTF-8 sequence; bytes when invalid)
	local function cp_len(s)
		return utf8.len(s) or #s
	end
	local function cp_head(s, n)
		return mare.text.cut(s, n, { unit = "chars" })
	end
	local function cp_tail(s, n)
		return mare.text.cut(s, n, { keep = "tail", unit = "chars" })
	end
	local function id_of(m)
		return S.svc.session and S.svc.session:entry_id(D.original(m)) or nil
	end
	local function session_path()
		return S.svc.session and S.svc.session:info().path or nil
	end

	-- ---- summary requests ---------------------------------------------------------------

	-- A switch summarizes with its committed source; a configured helper may itself have
	-- too small a window. Other compactions keep the configured summary-model policy.
	local function summary_model()
		return not S.target_model and cfg.summary_model and ctx.llm:resolve(cfg.summary_model) or ctx.llm:current()
	end

	-- Acknowledgement-only replies that models sometimes emit instead of a summary.
	local ACK_ONLY = {
		ack = true,
		acknowledged = true,
		ok = true,
		okay = true,
		done = true,
		understood = true,
		["got it"] = true,
		sure = true,
		yes = true,
		yep = true,
		yeah = true,
		noted = true,
		confirmed = true,
		ready = true,
		copy = true,
		["copy that"] = true,
		roger = true,
		["roger that"] = true,
		["will do"] = true,
		["sounds good"] = true,
		["all set"] = true,
		["no problem"] = true,
		thanks = true,
		["thank you"] = true,
		compacted = true,
		["summary complete"] = true,
		["compaction complete"] = true,
		["i understand"] = true,
		["i'll do that"] = true,
		["i will do that"] = true,
		good = true,
		fine = true,
		alright = true,
		["all right"] = true,
		right = true,
		great = true,
		perfect = true,
		cool = true,
		["n/a"] = true,
		none = true,
		["好的"] = true,
		["收到"] = true,
		["明白"] = true,
		["知道了"] = true,
		["了解"] = true,
		["完成"] = true,
		["没问题"] = true,
		["确认"] = true,
	}

	local function normalize_ack_clause(s)
		s = s:gsub("^[%s%p]+", ""):gsub("[%s%p]+$", "")
		s = s:gsub("[%s]+", " ")
		return s:lower()
	end

	local function strip_body_prefix(line)
		local s = line:match("^%s*(.-)%s*$")
		-- Strip markdown bullet/numbered list and task checkbox prefixes.
		s = s:gsub("^[-*+]%s+", "")
		s = s:gsub("^%d+[.)]%s+", "")
		s = s:gsub("^%[[xX%s]%]%s+", "")
		return s:match("^%s*(.-)%s*$")
	end

	local function is_ack_only_line(line)
		local stripped = strip_body_prefix(line)
		if stripped == "" then
			return true
		end
		-- Literal template placeholders like "[Completed tasks/changes]" are not substantive.
		if stripped:match("^%[[^%]]+%]$") then
			return true
		end
		local norm = normalize_ack_clause(stripped)
		if norm == "" then
			return true
		end
		if ACK_ONLY[norm] then
			return true
		end
		-- Handle chained short acknowledgements like "OK, understood." or "Got it, thanks!"
		local clauses, matched = 0, 0
		for part in stripped:gmatch("[^,!.;:]+") do
			local c = normalize_ack_clause(part)
			if c ~= "" then
				clauses = clauses + 1
				if ACK_ONLY[c] then
					matched = matched + 1
				end
			end
		end
		return clauses > 0 and clauses == matched
	end

	--- Validate that a summary response contains substantive body content (not empty,
	--- headings-only, or acknowledgement-only lines such as "ACK\nOK" or "## Goal\nOK")
	--- against the selected summary prompt.
	local function validate_summary(text, instruction)
		if type(text) ~= "string" or not text:find("%S") then
			return false, "summary came back empty"
		end
		local trimmed = text:match("^%s*(.-)%s*$")
		if not trimmed:find("[%w\128-\255]") then
			return false, "summary contained no substantive content"
		end
		local has_heading = false
		local body_lines = {}
		for line in trimmed:gmatch("[^\r\n]+") do
			if line:match("^%s*#+%s+%S") or line:match("^%s*#+%s*$") then
				has_heading = true
			elseif line:find("[%w\128-\255]") then
				body_lines[#body_lines + 1] = line
			end
		end
		if #body_lines == 0 then
			return false,
				has_heading and "summary contained headings with no substantive body"
					or "summary contained no substantive content"
		end
		local has_substantive_body = false
		for _, line in ipairs(body_lines) do
			if not is_ack_only_line(line) then
				has_substantive_body = true
				break
			end
		end
		if not has_substantive_body then
			return false, "summary was an acknowledgement instead of a substantive summary"
		end
		if not has_heading then
			local expects_sections = type(instruction) == "string" and instruction:find("\n## ", 1, true) ~= nil
			if expects_sections and #body_lines == 1 then
				local _, words = body_lines[1]:gsub("%S+", "")
				if words <= 1 and #body_lines[1] < 8 then
					return false, "summary was too short to satisfy the structured summary prompt"
				end
			end
		end
		return true
	end

	--- one summary request; checks the result (empty / truncated / degenerate / longer than its input)
	local function request(req, input_tokens, instruction)
		req.model = req.model or summary_model()
		req.purpose = "compact" -- llm/usage: counted apart from the conversation
		req.cancel = S.token
		req.cache = "read" -- reuse the prompt cache, don't write it
		req.session_id = S.svc.session and S.svc.session:info().session_id or nil
		local res = ctx.llm:complete(req)
		if res.stop_reason == "aborted" then
			return nil, "compaction cancelled", "cancelled"
		end
		if res.stop_reason == "error" then
			local kind = res.error_kind
			if kind == "retry_delay_too_long" and res.original_error_kind then
				kind = res.original_error_kind
			end
			return nil, res.error or "summarization failed", kind, res.fallback_worthy
		end
		if res.stop_reason == "length" then
			return nil, "summary hit the token cap and is incomplete"
		end
		-- a filtered answer is a refusal, whatever text came with it: never a summary
		if res.stop_reason == "filtered" then
			return nil, "the summary was filtered by the provider", "policy"
		end
		for _, b in ipairs(res.content or {}) do
			if b.type == "tool_call" and not b.hosted then
				return nil, "the summarizer called a tool", "tool_call"
			end
		end
		if res.stop_reason ~= "stop" then
			return nil,
				("summary did not complete cleanly (%s)"):format(tostring(res.stop_reason or "unknown")),
				res.stop_reason
		end
		local text = ctx.llm:text_of(res)
		local valid, verr = validate_summary(text, instruction)
		if not valid then
			return nil, verr, "invalid_summary"
		end
		if input_tokens and tokens(text) > input_tokens then
			return nil, "summary is longer than what it summarizes", "nonshrinking"
		end
		return text
	end

	--- Reorder out-of-place `tool_result` messages (e.g., earlier orphaned calls settled at the
	--- tail of the log) so every result sits immediately after its owning `assistant` message.
	--- Orphan results whose call is absent from `msgs` are dropped; genuinely unanswered tool
	--- calls stay unanswered so `missing_result` still detects corrupt history.
	local function repair_tool_order(msgs)
		local calls, results_by_id = {}, {}
		for _, m in ipairs(msgs) do
			if m.role == "assistant" then
				for _, b in ipairs(m.content or {}) do
					if b.type == "tool_call" and not b.hosted and b.id then
						calls[b.id] = true
					end
				end
			end
		end
		for _, m in ipairs(msgs) do
			if
				m.role == "tool_result"
				and m.tool_call_id
				and calls[m.tool_call_id]
				and not results_by_id[m.tool_call_id]
			then
				results_by_id[m.tool_call_id] = m
			end
		end
		local out, emitted = {}, {}
		for _, m in ipairs(msgs) do
			if m.role ~= "tool_result" then
				out[#out + 1] = m
				if m.role == "assistant" then
					for _, b in ipairs(m.content or {}) do
						if b.type == "tool_call" and not b.hosted and b.id and not emitted[b.id] then
							emitted[b.id] = true
							local r = results_by_id[b.id]
							if r then
								out[#out + 1] = r
							end
						end
					end
				end
			end
		end
		if #out == #msgs then
			local same = true
			for i = 1, #msgs do
				if out[i] ~= msgs[i] then
					same = false
					break
				end
			end
			if same then
				return msgs
			end
		end
		return out
	end

	--- protocol units: an assistant message with tool calls and the results of those calls form
	--- one unit — dropped together, never apart
	local function units_of(msgs)
		msgs = repair_tool_order(msgs)
		local units, i = {}, 1
		while i <= #msgs do
			local m = msgs[i]
			local u, j = { m }, i + 1
			if m.role == "assistant" then
				local ids = {}
				for _, b in ipairs(m.content or {}) do
					if b.type == "tool_call" and not b.hosted and b.id then
						ids[b.id] = true
					end
				end
				while j <= #msgs and msgs[j].role == "tool_result" and ids[msgs[j].tool_call_id or ""] do
					u[#u + 1] = msgs[j]
					j = j + 1
				end
			end
			units[#units + 1] = u
			i = j
		end
		return units
	end
	--- flatten units into messages, checking call/result pairing first: a result whose call
	--- isn't in the request is removed (the API rejects it)
	local function flatten(units)
		local out, calls = {}, {}
		for _, u in ipairs(units) do
			for _, m in ipairs(u) do
				if m.role == "assistant" then
					for _, b in ipairs(m.content or {}) do
						if b.type == "tool_call" and not b.hosted and b.id then
							calls[b.id] = true
						end
					end
					out[#out + 1] = m
				elseif m.role == "tool_result" then
					if calls[m.tool_call_id or ""] then
						out[#out + 1] = m
					end
				else
					out[#out + 1] = m
				end
			end
		end
		return out
	end

	--- A tool call without a result cannot be replayed as valid provider history. Fail rather
	--- than sending an incomplete request or silently removing content from the summary.
	local function missing_result(msgs)
		local calls, results = {}, {}
		for _, m in ipairs(msgs) do
			if m.role == "assistant" then
				for _, b in ipairs(m.content or {}) do
					if b.type == "tool_call" and not b.hosted then
						if type(b.id) ~= "string" or b.id == "" then
							return "<missing id>"
						end
						calls[b.id] = true
					end
				end
			elseif m.role == "tool_result" and m.tool_call_id then
				results[m.tool_call_id] = true
			end
		end
		for id in pairs(calls) do
			if not results[id] then
				return id
			end
		end
	end

	-- A summary request that doesn't fit first cuts each tool result (in the request copy only)
	-- to this many characters, then drops oldest units. Failed tool results keep both head and
	-- tail within the same budget so final build/test diagnostics survive.
	local SUMMARY_TOOL_RESULT_CHARS = 2000
	local SUMMARY_ERROR_HEAD_CHARS = 800
	local SUMMARY_ERROR_TAIL_CHARS = 1200
	local function trim_result(m)
		if m.role ~= "tool_result" then
			return m
		end
		local content, cut = {}, false
		for i, b in ipairs(m.content or {}) do
			local len = b.type == "text" and cp_len(b.text or "") or 0
			if b.type == "text" and len > SUMMARY_TOOL_RESULT_CHARS then
				local text
				if m.is_error then
					local omitted = len - SUMMARY_ERROR_HEAD_CHARS - SUMMARY_ERROR_TAIL_CHARS
					text = cp_head(b.text, SUMMARY_ERROR_HEAD_CHARS)
						.. ("\n\n[... %d characters omitted ...]\n\n"):format(omitted)
						.. cp_tail(b.text, SUMMARY_ERROR_TAIL_CHARS)
				else
					text = cp_head(b.text, SUMMARY_TOOL_RESULT_CHARS)
						.. ("\n\n[... %d more characters truncated]"):format(len - SUMMARY_TOOL_RESULT_CHARS)
				end
				content[i] = { type = "text", text = text }
				cut = true
			else
				content[i] = b
			end
		end
		if not cut then
			return m
		end
		local c = table.copy(m)
		c.content = content
		return c
	end

	local FALLBACK_KINDS = {
		context_overflow = true,
		too_large = true,
		payload_too_large = true,
		rate_limit = true,
		quota = true,
		usage_limit = true,
		invalid_summary = true,
	}

	local function strip_images(m)
		local content, changed = {}, false
		for i, b in ipairs(m.content or {}) do
			if type(b) == "table" and b.type == "image" then
				content[i] = { type = "text", text = "[image omitted during compaction]" }
				changed = true
			else
				content[i] = b
			end
		end
		if not changed then
			return m
		end
		local c = table.copy(m)
		c.content = content
		return c
	end

	local function has_images_in_units(units)
		for _, u in ipairs(units) do
			for _, m in ipairs(u) do
				for _, b in ipairs(m.content or {}) do
					if type(b) == "table" and b.type == "image" then
						return true
					end
				end
			end
		end
		return false
	end

	local LIVE_SHRINK_STEPS = { 0.85, 0.70, 0.50, 0.35 }

	local function summarize_with_model(model, prefix, units_in, instruction, allow_fallback, opts)
		local b, berr = budgets(model)
		if not b then
			return nil, berr, "context_overflow"
		end
		local units = {}
		for i, u in ipairs(units_in) do
			units[i] = u
		end
		local out_cap = math.min(cfg.summary_max_tokens or b.headroom, model.max_output or b.headroom)
		local input_budget = b.window - out_cap
		local system = S.svc.context and S.svc.context:system_prompt(model) or SYSTEM
		local tools = S.svc.tools and S.svc.tools:definitions(model) or {}
		local instr = { role = "user", content = { { type = "text", text = instruction } } }
		local dropped, without_tools, trimmed, images_stripped = {}, false, false, false
		local live_overflow_step = 0
		while true do
			local msgs = table.copy(prefix)
			for _, m in ipairs(flatten(units)) do
				local msg_out = m
				if trimmed then
					msg_out = trim_result(msg_out)
				end
				if images_stripped then
					msg_out = strip_images(msg_out)
				end
				msgs[#msgs + 1] = msg_out
			end
			local missing = missing_result(msgs)
			if missing then
				return nil, "summary request has a tool call without a result: " .. tostring(missing), "invalid_history"
			end
			msgs[#msgs + 1] = instr
			local extra = tokens(system or "") + (#tools > 0 and tokens(tools) or 0)
			local need = estimate(msgs) + extra
			local s, err, kind, worthy
			local preflight_overflow = false
			if need > input_budget then
				preflight_overflow = true
				kind = "context_overflow"
				err = "the summary request is longer than the window"
			else
				local input_limit = not (opts and opts.allow_expansion) and (need - extra) or nil
				s, err, kind, worthy = request({
					model = model,
					system = system,
					tools = tools,
					tool_choice = "none",
					max_tokens = out_cap,
					no_fallback = allow_fallback or nil,
					messages = msgs,
				}, input_limit, instruction)
			end
			if s then
				return s, dropped
			end
			local media_overflow = kind == "image_limit"
				or (kind == "payload_too_large" and not images_stripped and has_images_in_units(units))
			local is_overflow = kind == "context_overflow" or kind == "payload_too_large" or kind == "image_limit"
			if kind == "tool_call" and #tools > 0 and not without_tools then
				tools, without_tools = {}, true
			elseif media_overflow and not images_stripped then
				images_stripped = true
			elseif allow_fallback and (FALLBACK_KINDS[kind] or worthy) then
				-- `no_fallback` held back `fallback_models`; the current model's attempt has them
				return nil, err, kind, true
			else
				local trimmable = false
				if is_overflow and not trimmed then
					for _, m in ipairs(flatten(units)) do
						trimmable = trimmable or trim_result(m) ~= m
					end
				end
				if trimmable then
					trimmed = true
				elseif is_overflow and preflight_overflow and #units > 1 then
					dropped[#dropped + 1] = table.remove(units, 1)
				elseif
					is_overflow
					and not preflight_overflow
					and #units > 1
					and live_overflow_step < #LIVE_SHRINK_STEPS
				then
					live_overflow_step = live_overflow_step + 1
					local keep = math.max(1, math.floor(#units * LIVE_SHRINK_STEPS[live_overflow_step]))
					local drop_n = math.max(1, #units - keep)
					for _ = 1, drop_n do
						dropped[#dropped + 1] = table.remove(units, 1)
					end
				elseif is_overflow and #units == 1 then
					local last = units[1]
					local last_m = last[1]
					local t = last_m and msg_tokens(trim_result(last_m)) or need
					return nil,
						("message %s is %d tokens; the summary model's window allows %d — nothing replaced"):format(
							(last_m and id_of(last_m)) or ("(" .. ((last_m and last_m.role) or "unit") .. ")"),
							t,
							input_budget
						),
						"too_large"
				else
					return nil, err, kind
				end
			end
		end
	end

	--- Summarize `units`: the request replays
	--- the current system prompt, tool declarations and the protected prefix (earlier summary,
	--- preserved user messages) followed by the units, with the instruction as the last user
	--- message; tool_choice = none (once more without tools if the model still calls one). Too
	--- long → cut every tool result to 2000 characters, then drop the oldest unit and retry
	--- `prefix` is never dropped. If a configured summary model
	--- fails due to window or limit/quota — or an error `fallback_models` answer (overloaded, missing
	--- model) — retry once with the current model, whose request may still use `fallback_models`:
	--- summary model → current model → fallback_models (never on policy refusal or cancellation;
	--- usage is recorded for every attempt). Returns summary, dropped
	--- (units) | nil, err, kind.
	local function summarize(prefix, units, instruction, opts)
		local current = ctx.llm:current()
		local primary = summary_model()
		local can_fallback = current ~= nil and primary ~= nil and primary.ref ~= current.ref
		local s, dropped, kind, handoff = summarize_with_model(primary, prefix, units, instruction, can_fallback, opts)
		if s then
			return s, dropped
		end
		if can_fallback and (FALLBACK_KINDS[kind] or handoff) and not (S.token and S.token:is_cancelled()) then
			mare.log(
				"warn",
				("summary model %s failed (%s); retrying with current model %s"):format(
					primary.ref,
					kind or tostring(dropped),
					current.ref
				)
			)
			local s2, dropped2, kind2 = summarize_with_model(current, prefix, units, instruction, false, opts)
			if s2 then
				return s2, dropped2
			end
			dropped, kind = dropped2, kind2
		end
		local alt =
			ctx.bus:waterfall("compact/summary-error", { error = dropped, kind = kind, messages = flatten(units) })
		if type(alt) == "table" and validate_summary(alt.summary, instruction) then
			return alt.summary, {}
		end
		return nil, dropped, kind
	end

	return {
		text_of = text_of,
		cp_len = cp_len,
		cp_head = cp_head,
		cp_tail = cp_tail,
		id_of = id_of,
		session_path = session_path,
		repair_tool_order = repair_tool_order,
		units_of = units_of,
		summarize = summarize,
	}
end
