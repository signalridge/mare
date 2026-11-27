-- provider-openai-compat.wire: Chat Completions wire helpers (message/tool conversion,
-- listed-model decoding, usage parsing, SSE stream parser, error classification, prompt cache
-- breakpoints); pure module returning its table `M` directly.

-- too many / too large images in one request (Chat Completions servers: OpenRouter passes the
-- upstream wording through / local servers use OpenAI's)
local function IMAGE_LIMIT(msg, code)
	local m = string.lower(tostring(msg or ""))
	return code == "image_too_large"
		or m:find("too many images", 1, true) ~= nil
		or m:find("image too large", 1, true) ~= nil
		or m:find("image is too large", 1, true) ~= nil
		or m:find("image exceeds", 1, true) ~= nil
		or m:find("number of images", 1, true) ~= nil
end

local function OVERFLOW(msg)
	local m = string.lower(tostring(msg or ""))
	return m:find("prompt is too long", 1, true) ~= nil
		or m:find("context_length_exceeded", 1, true) ~= nil
		or m:find("maximum context length", 1, true) ~= nil
		or m:find("context window", 1, true) ~= nil
		or m:find("exceeds the maximum number of tokens", 1, true) ~= nil
		or m:find("input token count", 1, true) ~= nil
end

local COMPAT_CODES = {
	insufficient_quota = "quota",
	exceeded_current_quota_error = "quota",
	rate_limit_exceeded = "rate_limit",
	server_is_overloaded = "overloaded",
	slow_down = "overloaded",
}

local function cerebras_overflow_map(raw)
	local st = raw.status
	local body_str = tostring(raw.body or raw.raw_message or "")
	local m = string.lower(tostring(raw.raw_message or ""))
	if (st == 400 or st == 413) and body_str:match("^%s*$") then
		return { kind = "context_overflow", retryable = false }
	end
	if m:match("^4[01][03]%s*status code%s*%(no body%)") or m:match("^4[01][03]%s*%(no body%)") then
		return { kind = "context_overflow", retryable = false }
	end
end

local function ephemeral_cache_control(marker)
	if type(marker) == "table" then
		return { type = marker.type or "ephemeral", ttl = marker.ttl }
	end
	return { type = "ephemeral" }
end

local function add_cache_control_to_text_content(msg, marker)
	local content = msg.content
	if type(content) == "string" then
		if content == "" then
			return false
		end
		msg.content = { { type = "text", text = content, cache_control = ephemeral_cache_control(marker) } }
		return true
	end
	if type(content) ~= "table" then
		return false
	end
	for i = #content, 1, -1 do
		local part = content[i]
		if type(part) == "table" and part.type == "text" then
			part.cache_control = ephemeral_cache_control(marker)
			return true
		end
	end
	return false
end

--- Anthropic-style prompt-cache breakpoints on Chat Completions:
--- system prompt, last tool definition, and last conversation message with non-empty text.
local function apply_anthropic_cache_control(messages, tools, marker)
	for _, msg in ipairs(messages or {}) do
		if msg.role == "system" or msg.role == "developer" then
			add_cache_control_to_text_content(msg, marker)
			break
		end
	end
	if type(tools) == "table" and #tools > 0 then
		tools[#tools].cache_control = ephemeral_cache_control(marker)
	end
	for i = #(messages or {}), 1, -1 do
		local msg = messages[i]
		if msg.role == "user" or msg.role == "assistant" or msg.role == "tool" then
			if add_cache_control_to_text_content(msg, marker) then
				break
			end
		end
	end
end

local function is_openai_host(url)
	if type(url) ~= "string" or url == "" then
		return false
	end
	local ok, normalized = pcall(mare.http.build_url, url)
	if not ok or type(normalized) ~= "string" then
		return false
	end
	local authority = normalized:match("^https?://([^/?#]+)")
	if not authority then
		return false
	end
	authority = authority:match("([^@]+)$") or authority
	local host = authority:match("^([^:]+):?%d*$")
	return host == "api.openai.com"
end

--- Clamp prompt_cache_key to 64 characters.
local function clamp_prompt_cache_key(key)
	if type(key) ~= "string" or key == "" then
		return nil
	end
	local len = utf8.len(key)
	if not len then
		return #key <= 64 and key or key:sub(1, 64)
	end
	if len <= 64 then
		return key
	end
	local end_byte = utf8.offset(key, 65)
	return end_byte and key:sub(1, end_byte - 1) or key
end

local WIRE = "openai-chat"

---@return table<string, any>|nil
local function native_of(obj)
	return type(obj) == "table"
			and type(obj.native) == "table"
			and type(obj.native[WIRE]) == "table"
			and obj.native[WIRE]
		or nil
end

local M = {
	IMAGE_LIMIT = IMAGE_LIMIT,
	OVERFLOW = OVERFLOW,
	COMPAT_CODES = COMPAT_CODES,
	cerebras_overflow_map = cerebras_overflow_map,
	apply_anthropic_cache_control = apply_anthropic_cache_control,
	is_openai_host = is_openai_host,
	clamp_prompt_cache_key = clamp_prompt_cache_key,
}

--- Cheaply decide if leaked tool-call markup healing applies before loading markup_healing.lua.
function M.resolve_markup_healing(ep, model, base_url)
	local explicit = (type(model) == "table" and model.markup_healing)
		or (type(ep) == "table" and type(ep.endpoint_config) == "table" and ep.endpoint_config.markup_healing)
		or (type(ep) == "table" and type(ep.provider_config) == "table" and ep.provider_config.markup_healing)
		or (type(ep) == "table" and ep.markup_healing)
	if explicit == false or explicit == "none" or explicit == "off" then
		return nil
	end
	if type(explicit) == "string" then
		return require("provider-openai-compat.markup_healing").resolve_pattern(ep, model, base_url)
	end
	local d = type(ep) == "table" and type(ep.dialect) == "string" and ep.dialect:lower() or ""
	local url = type(base_url) == "string" and base_url:lower() or ""
	local mid = type(model) == "table" and type(model.id) == "string" and model.id:lower()
		or (type(model) == "string" and model:lower() or "")
	local maybe = d == "deepseek"
		or d == "kimi"
		or d == "moonshot"
		or d == "qwen"
		or d == "qwen-chat-template"
		or url:find("deepseek", 1, true)
		or url:find("kimi", 1, true)
		or url:find("moonshot", 1, true)
		or url:find("dashscope", 1, true)
		or mid:find("deepseek", 1, true)
		or mid:find("kimi", 1, true)
		or mid:find("moonshot", 1, true)
		or mid:find("qwen", 1, true)
	if not maybe then
		return nil
	end
	return require("provider-openai-compat.markup_healing").resolve_pattern(ep, model, base_url)
end

--- The reasoning_details entries kept and sent back (OpenRouter reasoning-details format): the
--- fields kept for each recognised type, plus `id` / `format` / `index`. One table for parsing,
--- the thinking block and the replay, so no field is dropped on the way. Unknown types are
--- dropped: echoing an undocumented payload is what breaks the follow-up request.
local DETAIL_FIELDS = {
	["reasoning.text"] = { "text", "signature" },
	["reasoning.summary"] = { "summary" },
	["reasoning.encrypted"] = { "data" },
}
local DETAIL_KEPT = { "id", "format", "index" }

--- A reasoning_details entry with exactly the fields its type keeps; nil for an unknown type.
local function copy_detail(d)
	local fields = type(d) == "table" and DETAIL_FIELDS[d.type]
	if not fields then
		return nil
	end
	local out = { type = d.type }
	for _, k in ipairs(fields) do
		out[k] = d[k]
	end
	for _, k in ipairs(DETAIL_KEPT) do
		out[k] = d[k]
	end
	return out
end

--- Only consecutive compatible text or summary entries merge; encrypted entries are opaque and are
--- always kept individually. A kept field (`text`, `summary`, `signature`, `data`) that is not a
--- string is skipped before merging: what was already received stays, and the response goes on.
local function append_detail(details, raw)
	local d = { type = raw.type }
	for _, k in ipairs(DETAIL_FIELDS[raw.type]) do
		if type(raw[k]) == "string" then
			d[k] = raw[k]
		end
	end
	for _, k in ipairs(DETAIL_KEPT) do
		d[k] = raw[k]
	end
	local previous = details[#details]
	local merge = previous and previous.type == d.type and (d.type == "reasoning.text" or d.type == "reasoning.summary")
	if merge then
		for _, k in ipairs(DETAIL_KEPT) do
			if previous[k] ~= nil and d[k] ~= nil and previous[k] ~= d[k] then
				merge = false
				break
			end
		end
		local old_sig, new_sig = previous.signature, d.signature
		if old_sig and old_sig ~= "" and new_sig and new_sig ~= "" and old_sig ~= new_sig then
			merge = false
		end
	end
	if not merge then
		details[#details + 1] = copy_detail(d)
		return
	end
	local text = d.type == "reasoning.text" and "text" or "summary"
	previous[text] = (previous[text] or "") .. (d[text] or "")
	for _, k in ipairs({ "id", "index" }) do
		if d[k] ~= nil then
			previous[k] = d[k]
		end
	end
	-- The first non-empty signature and format win, including when they arrive after the text.
	for _, k in ipairs({ "signature", "format" }) do
		if previous[k] == nil or previous[k] == "" then
			previous[k] = d[k] or previous[k]
		end
	end
end

--- One entry of GET /models, keeping what the endpoint says about the model: the
--- context window, output limit, input modalities, reasoning and price (OpenRouter's
--- `architecture.input_modalities`, `supported_parameters`, `top_provider`, per-token `pricing`);
--- anything missing is filled from models.dev, then conservative defaults, by the llm plugin.
function M.listed_model(m)
	local arch = type(m.architecture) == "table" and m.architecture or {}
	local top = type(m.top_provider) == "table" and m.top_provider or {}
	local out = {
		id = m.id,
		name = type(m.name) == "string" and m.name or nil,
		context_window = tonumber(m.context_length or m.context_window or top.context_length),
		max_output = tonumber(m.max_output_tokens or m.max_completion_tokens or top.max_completion_tokens),
	}
	local mods = arch.input_modalities or m.input_modalities
	if type(mods) == "table" then
		local inp = {}
		for _, x in ipairs(mods) do
			if x == "text" or x == "image" then
				inp[#inp + 1] = x
			end
		end
		out.input = #inp > 0 and inp or nil
	end
	if type(m.supported_parameters) == "table" then
		out.reasoning = false
		for _, p in ipairs(m.supported_parameters) do
			if p == "reasoning" or p == "reasoning_effort" or p == "include_reasoning" then
				out.reasoning = true
			end
		end
	end
	local pr = type(m.pricing) == "table" and m.pricing or nil
	if pr and tonumber(pr.prompt) and tonumber(pr.completion) then
		-- per token -> per million tokens
		out.cost = {
			input = tonumber(pr.prompt) * 1e6,
			output = tonumber(pr.completion) * 1e6,
			cache_read = tonumber(pr.input_cache_read) and tonumber(pr.input_cache_read) * 1e6 or nil,
			cache_write = tonumber(pr.input_cache_write) and tonumber(pr.input_cache_write) * 1e6 or nil,
		}
	end
	if m.dynamically_loaded_tools == true or m.supports_dynamically_loaded_tools == true then
		out.tool_update = "addition_only"
	elseif type(m.capabilities) == "table" then
		for _, c in ipairs(m.capabilities) do
			if type(c) == "string" and c:lower() == "dynamically_loaded_tools" then
				out.tool_update = "addition_only"
				break
			end
		end
	end
	return out
end

function M.convert_messages(system, messages, endpoint, opts)
	opts = opts or {}
	local placeholder_reasoning = opts.placeholder_reasoning == true
	local heal_pattern = type(opts.markup_healing) == "string" and opts.markup_healing or nil
	local healer_mod = heal_pattern and require("provider-openai-compat.markup_healing") or nil
	local after_tool = false
	local undeclared
	if type(opts.deferred_tools) == "table" then
		undeclared = {}
		for k, v in pairs(opts.deferred_tools) do
			if v then
				undeclared[k] = v
			end
		end
	end
	local out = {}
	if system and system ~= "" then
		out[#out + 1] = { role = "system", content = system }
	end
	local pending_tool_additions = {}
	local function flush_tool_additions()
		if #pending_tool_additions == 0 then
			return
		end
		-- Kimi mid-conversation tool declarations: `{ role = "system", tools = [...] }` with
		-- no `content` field, flushed right after the tool_result batch to keep the prefix stable.
		out[#out + 1] = { role = "system", tools = mare.json.array(pending_tool_additions) }
		pending_tool_additions = {}
	end
	local pending_tool_images = {}
	local function flush_tool_images()
		if #pending_tool_images == 0 then
			return
		end
		local parts = { { type = "text", text = "Attached image(s) from tool result:" } }
		for _, img in ipairs(pending_tool_images) do
			parts[#parts + 1] = img
		end
		out[#out + 1] = { role = "user", content = mare.json.array(parts) }
		pending_tool_images = {}
	end
	for _, m in ipairs(messages) do
		if m.role == "user" then
			flush_tool_additions()
			flush_tool_images()
			after_tool = false
			local parts, has_image = {}, false
			for _, b in ipairs(m.content) do
				if b.type == "text" then
					parts[#parts + 1] = { type = "text", text = b.text }
				elseif b.type == "mention" then
					local label = b.name and ("@" .. b.path .. " (" .. b.name .. ")") or ("@" .. b.path)
					parts[#parts + 1] = { type = "text", text = label }
				elseif b.type == "image" then
					has_image = true
					parts[#parts + 1] = {
						type = "image_url",
						image_url = { url = ("data:%s;base64,%s"):format(b.mime or "image/png", b.data) },
					}
				end
			end
			if has_image then
				out[#out + 1] = { role = "user", content = parts }
			else
				local texts = {}
				for _, p in ipairs(parts) do
					texts[#texts + 1] = p.text
				end
				out[#out + 1] = { role = "user", content = table.concat(texts, "\n") }
			end
		elseif m.role == "assistant" then
			flush_tool_additions()
			flush_tool_images()
			local same = m.provider_id == endpoint
			local m_nat = same and native_of(m) or nil
			local thought_signature = m_nat and m_nat.thought_signature or nil
			local texts, calls, reasoning, reasoning_block = {}, {}, {}, nil
			for _, b in ipairs(m.content) do
				if b.type == "text" then
					local txt = healer_mod and healer_mod.strip_leaked_text(heal_pattern, b.text) or b.text
					if txt ~= "" then
						texts[#texts + 1] = txt
					end
				elseif b.type == "reasoning" then
					local rt = healer_mod and healer_mod.strip_leaked_text(heal_pattern, b.text) or b.text
					if rt ~= "" then
						reasoning[#reasoning + 1] = rt
					end
					reasoning_block = reasoning_block or b
				elseif b.type == "tool_call" and not b.hosted then
					local clean_args = b.arguments or {}
					if healer_mod and not b.raw_arguments then
						clean_args = healer_mod.clean_arguments(heal_pattern, clean_args)
					end
					local call = {
						id = b.id,
						type = "function",
						["function"] = {
							name = b.name,
							arguments = b.raw_arguments or mare.json.encode(clean_args),
						},
					}
					-- a call's thought signature goes back where it came (Gemini needs it)
					local bn = same and native_of(b) or nil
					local sig = bn and bn.thought_signature
					if type(sig) == "string" and sig ~= "" then
						if bn.signature_field == "thought_signature" then
							call.thought_signature = sig
						else
							call.extra_content = { google = { thought_signature = sig } }
						end
					end
					calls[#calls + 1] = call
				end
			end
			local msg = { role = "assistant", content = table.concat(texts) }
			if type(thought_signature) == "string" and thought_signature ~= "" then
				msg.thought_signature = thought_signature
			end
			if #calls > 0 then
				msg.tool_calls = calls
			end
			-- DeepSeek-style reasoning with tools needs the reasoning passed back (same endpoint, or
			-- a single-space placeholder when missing/cross-provider so relays don't strip it)
			if (#reasoning > 0 or reasoning_block ~= nil) and same then
				local rc = table.concat(reasoning)
				local rn = reasoning_block and native_of(reasoning_block) or nil
				local field = (rn and rn.reasoning_field) or "reasoning_content"
				if rc ~= "" and field ~= "reasoning_details" then
					msg[field == "reasoning" and "reasoning" or "reasoning_content"] = rc
				end
				-- the reasoning_details a gateway sent go back as received: every entry, in order
				local details = rn and rn.reasoning_details
				if type(details) == "table" and #details > 0 then
					local out_details = {}
					for _, d in ipairs(details) do
						local cd = copy_detail(d)
						if cd and healer_mod then
							if type(cd.text) == "string" then
								cd.text = healer_mod.strip_leaked_text(heal_pattern, cd.text)
							end
							if type(cd.summary) == "string" then
								cd.summary = healer_mod.strip_leaked_text(heal_pattern, cd.summary)
							end
						end
						out_details[#out_details + 1] = cd
					end
					msg.reasoning_details = out_details
				end
			end
			if
				placeholder_reasoning
				and msg.reasoning_content == nil
				and (#calls > 0 or after_tool or opts.has_tools)
			then
				msg.reasoning_content = " "
			end
			out[#out + 1] = msg
		elseif m.role == "tool_result" then
			after_tool = true
			local parts, has_images = {}, false
			for _, b in ipairs(m.content) do
				if b.type == "text" then
					parts[#parts + 1] = b.text
				elseif b.type == "image" then
					has_images = true
					pending_tool_images[#pending_tool_images + 1] = {
						type = "image_url",
						image_url = { url = ("data:%s;base64,%s"):format(b.mime or "image/png", b.data) },
					}
				end
			end
			local text_result = table.concat(parts, "\n")
			local has_text = text_result:match("%S") ~= nil
			local content = has_text and text_result or (has_images and "(see attached image)" or "(no tool output)")
			local tool_msg = { role = "tool", tool_call_id = m.tool_call_id, content = content }
			out[#out + 1] = tool_msg
			local loaded = not m.is_error
				and undeclared
				and type(m.details) == "table"
				and type(m.details.loaded_tools) == "table"
				and m.details.loaded_tools
			if loaded then
				local activated = {}
				for _, name in ipairs(loaded) do
					local tool_def = undeclared[name]
					if tool_def then
						undeclared[name] = nil
						pending_tool_additions[#pending_tool_additions + 1] = tool_def
						activated[#activated + 1] = name
					end
				end
				if #activated > 0 then
					tool_msg.content = "Loaded: " .. table.concat(activated, ", ")
				end
			end
		end
	end
	flush_tool_additions()
	flush_tool_images()
	return out
end

function M.convert_tools(tools, dialect, opts)
	local out = {}
	local defer_loading = type(opts) == "table" and opts.defer_loading == true
	local deferred
	local sanitize = dialect and require("mare_lib.schema_sanitize").sanitize or nil
	for _, t in ipairs(tools or {}) do
		if t.type ~= "hosted" then
			local params = sanitize and sanitize(t.parameters, dialect) or t.parameters
			local entry = {
				type = "function",
				["function"] = { name = t.name, description = t.description, parameters = params },
			}
			if defer_loading and t.defer_loading then
				deferred = deferred or {}
				deferred[t.name] = entry
			else
				out[#out + 1] = entry
			end
		end
	end
	return out, deferred
end

--- Usage of a Chat Completions chunk: cache reads from prompt_tokens_details.cached_tokens
--- (OpenAI, OpenRouter), prompt_cache_hit_tokens (DeepSeek), top-level cached_tokens or
--- Anthropic-style cache_read_input_tokens (DigitalOcean); cache writes from
--- prompt_tokens_details.cache_write_tokens (OpenRouter) or Anthropic-style
--- cache_created_input_tokens / cache_creation_input_tokens (DigitalOcean), never subtracted from
--- the reads; input clamped at 0; completion_tokens already includes the reasoning tokens.
function M.parse_usage(u, opts)
	if type(u) ~= "table" or next(u) == nil then
		return nil
	end
	local details = type(u.prompt_tokens_details) == "table" and u.prompt_tokens_details or {}
	local cached = tonumber(details.cached_tokens)
		or tonumber(u.prompt_cache_hit_tokens)
		or tonumber(u.cached_tokens)
		or tonumber(u.cache_read_input_tokens)
		or 0
	local written = tonumber(details.cache_write_tokens)
		or tonumber(u.cache_created_input_tokens)
		or tonumber(u.cache_creation_input_tokens)
		or 0
	local cdetails = type(u.completion_tokens_details) == "table" and u.completion_tokens_details or {}
	local out = {
		input = math.max(0, (tonumber(u.prompt_tokens) or 0) - cached - written),
		output = tonumber(u.completion_tokens) or 0,
		cache_read = cached,
		cache_write = written,
		reasoning = tonumber(cdetails.reasoning_tokens) or tonumber(u.reasoning_tokens),
	}
	if opts and opts.is_openrouter then
		local reported = require("mare_lib").openrouter_cost(u)
		if reported ~= nil then
			out.cost = reported
		end
	end
	return out
end

--- new_parser(sink, on_error?, opts?) -- `on_error(api_err)` turns a mid-stream `{"error": ...}` into the
--- error raised at the end (the provider passes llm:stream_error so code / type / status count).
function M.new_parser(sink, on_error, opts)
	local stream = require("mare_lib")
	local P = stream.stream_record(
		{ text = "", thinking = "", calls = {}, by_index = {}, by_id = {}, usage = nil, finish = nil },
		{ "text", "thinking" }
	)
	P.order = P.calls -- calls in arrival order
	local missing_id_seq = 0
	local heal_pattern = type(opts) == "table" and type(opts.markup_healing) == "string" and opts.markup_healing or nil
	local healer_mod = heal_pattern and require("provider-openai-compat.markup_healing") or nil
	local healer = healer_mod and healer_mod.new_scanner(heal_pattern) or nil
	local healed_calls = {}
	local has_structured_calls = false

	local function mint_call_id(name, idx_tag)
		missing_id_seq = missing_id_seq + 1
		if P.record_id then
			local seed = ("%s:%s:%s"):format(P.record_id, tostring(name or ""), tostring(idx_tag or ""))
			local id
			id, missing_id_seq = stream.next_hashed_call_id(P.by_id, seed, 32, missing_id_seq)
			return id
		end
		local id
		repeat
			id = "call_" .. mare.crypto.random_hex(8)
		until not P.by_id[id]
		return id
	end

	local function emit_healed_events(events)
		for _, ev in ipairs(events) do
			if ev.type == "text" and ev.text ~= "" then
				stream.stream_append(P, "text", ev.text)
				sink({ type = "text_delta", delta = ev.text })
			elseif ev.type == "tool_call" and not has_structured_calls then
				local id = mint_call_id(ev.name, "healed:" .. tostring(#healed_calls + 1))
				local rec = {
					id = id,
					name = ev.name,
					args = ev.raw_arguments or mare.json.encode(ev.arguments or {}),
					healed_arguments = ev.arguments,
				}
				P.by_id[id] = rec
				healed_calls[#healed_calls + 1] = rec
				sink({ type = "tool_call_start", tool_call_id = id, name = ev.name })
				sink({ type = "tool_call_delta", tool_call_id = id, delta = rec.args })
			end
		end
	end

	local function append_visible_text(text)
		if type(text) ~= "string" or text == "" then
			return
		end
		if healer then
			emit_healed_events(healer:feed(text))
		else
			stream.stream_append(P, "text", text)
			sink({ type = "text_delta", delta = text })
		end
	end

	local function append_thinking_part(text)
		if type(text) == "string" and text ~= "" then
			stream.stream_append(P, "thinking", text)
			sink({ type = "reasoning_delta", delta = text })
		end
	end

	local function emit_content_array(parts)
		for _, part in ipairs(parts) do
			if type(part) == "table" then
				if part.type == "thinking" then
					if type(part.thinking) == "table" then
						for _, tp in ipairs(part.thinking) do
							if type(tp) == "table" and tp.type == "text" then
								append_thinking_part(tp.text)
							end
						end
					else
						append_thinking_part(type(part.thinking) == "string" and part.thinking or part.text)
					end
				elseif part.type == "text" and type(part.text) == "string" and part.text ~= "" then
					append_visible_text(part.text)
				end
			end
		end
	end

	--- The call a tool_calls delta continues: by index, then
	--- by id; a delta with neither continues the last call unless it names a new one.
	local function call_for(tc, fn)
		local id = type(tc.id) == "string" and tc.id ~= "" and tc.id or nil
		local cur = tc.index ~= nil and P.by_index[tc.index] or nil
		if not cur and id then
			cur = P.by_id[id]
		end
		if not cur and tc.index == nil and not id then
			local last = P.calls[#P.calls]
			local names_new = type(fn.name) == "string" and fn.name ~= "" and last and last.name ~= ""
			if last and not names_new then
				cur = last
			end
		end
		if not cur then
			cur = stream.stream_record({ name = "", args = "" }, { "args" })
			P.calls[#P.calls + 1] = cur
		end
		if tc.index ~= nil and not P.by_index[tc.index] then
			P.by_index[tc.index] = cur
		end
		if id and not cur.id then
			cur.id = id
		end
		if cur.id then
			P.by_id[cur.id] = cur
		end
		return cur
	end

	function P.feed(ev)
		-- skip keep-alive `data:` lines and the [DONE] sentinel before parsing; anything else that
		-- doesn't parse is a malformed stream, not silently dropped
		local data = ev.data or ""
		if data == "[DONE]" then
			return true
		end
		if not data:match("%S") then
			return false
		end
		local d = mare.json.decode(data)
		if type(d) ~= "table" then
			error({ message = "malformed stream event: " .. data:sub(1, 120), kind = "malformed_response" })
		end
		if type(d.id) == "string" and d.id ~= "" and not P.record_id then
			P.record_id = d.id
		end
		if d.error then
			P.error = type(d.error) == "table" and d.error or { message = tostring(d.error) }
			return true
		end
		local c = d.choices and d.choices[1]
		local raw_usage = (type(d.usage) == "table" and next(d.usage) ~= nil and d.usage)
			or (c and type(c.usage) == "table" and next(c.usage) ~= nil and c.usage)
			or nil
		if raw_usage then
			local parsed = M.parse_usage(raw_usage, opts)
			if parsed then
				P.usage = parsed
			end
		end
		if not c then
			return
		end
		local delta = c.delta or {}
		-- the field the reasoning text comes in is the one it goes back in (OpenRouter, Groq and
		-- Cerebras use `reasoning`, DeepSeek-style servers `reasoning_content`)
		local r, rfield = delta.reasoning_content, "reasoning_content"
		if type(r) ~= "string" or r == "" then
			r, rfield = delta.reasoning, "reasoning"
		end
		local has_r = type(r) == "string" and r ~= ""
		if has_r then
			P.reasoning_field = P.reasoning_field or rfield
			stream.stream_append(P, "thinking", r)
			sink({ type = "reasoning_delta", delta = r })
		end
		-- OpenRouter-format `reasoning_details` (OpenRouter; Venice in front of Claude, Gemini, GPT):
		-- Keep entries in arrival order for message-wide replay.
		-- Stream it until a scalar arrives; a later scalar may repeat earlier displayed details,
		-- but only the scalar is retained for replay when one arrives.
		if type(delta.reasoning_details) == "table" then
			P.details = P.details or {}
			for _, item in ipairs(delta.reasoning_details) do
				local fields = type(item) == "table" and DETAIL_FIELDS[item.type]
				if fields then
					append_detail(P.details, item)
					local shown = item.text or item.summary
					if type(shown) == "string" and shown ~= "" and not P.reasoning_field then
						sink({ type = "reasoning_delta", delta = shown })
					end
				end
			end
		end
		-- The signature can arrive in a final delta without content (Venice Gemini).
		if not P.thought_signature and type(delta.thought_signature) == "string" and delta.thought_signature ~= "" then
			P.thought_signature = delta.thought_signature
		end
		if type(delta.tool_calls) == "table" and #delta.tool_calls > 0 then
			has_structured_calls = true
		end
		local gextra = type(delta.extra_content) == "table"
				and type(delta.extra_content.google) == "table"
				and delta.extra_content.google
			or nil
		if type(delta.content) == "string" and delta.content ~= "" then
			if gextra and gextra.thought == true then
				append_thinking_part(delta.content)
			else
				append_visible_text(delta.content)
			end
		elseif type(delta.content) == "table" then
			emit_content_array(delta.content)
		end
		-- real servers send arguments before the name, or no id at all: buffer arguments until the
		-- name is known, synthesize an id, take the name the first time only (never concatenate
		-- repeats)
		local lossless_calls = delta.tool_calls and mare.json.decode_lossless(data).choices[1].delta.tool_calls
		for index, tc in ipairs(delta.tool_calls or {}) do
			local fn = tc["function"] or {}
			local raw_fn = lossless_calls[index]["function"] or {}
			local cur = call_for(tc, fn)
			-- a Gemini thought signature on the call: Google's own `extra_content.google`, or
			-- directly on the call (Venice); it goes back where it came from
			if not cur.signature then
				local g = type(tc.extra_content) == "table"
					and type(tc.extra_content.google) == "table"
					and tc.extra_content.google
				if type(tc.thought_signature) == "string" and tc.thought_signature ~= "" then
					cur.signature, cur.signature_field = tc.thought_signature, "thought_signature"
				elseif g and type(g.thought_signature) == "string" and g.thought_signature ~= "" then
					cur.signature, cur.signature_field = g.thought_signature, "extra_content"
				end
			end
			if type(fn.name) == "string" and fn.name ~= "" and cur.name == "" then
				cur.name = fn.name
				if not cur.id then
					missing_id_seq = missing_id_seq + 1
					if P.record_id then
						-- Deterministic across replays of the same response, <= 40 chars (Chat Completions limit)
						local seed = ("%s:%s:%s"):format(
							P.record_id,
							tostring(cur.name or ""),
							tostring(tc.index or "")
						)
						cur.id, missing_id_seq = stream.next_hashed_call_id(P.by_id, seed, 32, missing_id_seq)
					else
						local id
						repeat
							id = "call_" .. mare.crypto.random_hex(8)
						until not P.by_id[id]
						cur.id = id
					end
					P.by_id[cur.id] = cur
				end
				sink({ type = "tool_call_start", tool_call_id = cur.id, name = cur.name })
				if cur.args ~= "" then
					sink({ type = "tool_call_delta", tool_call_id = cur.id, delta = cur.args })
				end
			end
			if raw_fn.arguments ~= nil then
				cur.arguments_present = true
				if type(raw_fn.arguments) ~= "string" then
					cur.arguments_invalid = true
				end
			end
			if type(fn.arguments) == "string" and fn.arguments ~= "" then
				stream.stream_append(cur, "args", fn.arguments)
				if cur.name ~= "" then
					sink({ type = "tool_call_delta", tool_call_id = cur.id, delta = fn.arguments })
				end
			end
		end
		if type(c.finish_reason) == "string" and c.finish_reason ~= "" then
			P.finish = c.finish_reason
		end
	end

	function P.finish_message()
		if P.error then
			if on_error then
				error(on_error(P.error))
			end
			error({ message = P.error.message or mare.json.encode(P.error), code = P.error.code })
		end
		-- the end signal is a non-empty finish_reason, not [DONE]
		if not P.finish then
			error({ message = "stream closed before finish_reason", kind = "stream_closed" })
		end
		if P.finish == "network_error" then
			-- The upstream connection dropped mid-answer.
			error({ message = "provider reported finish_reason network_error", kind = "transport" })
		elseif P.finish == "error" then
			-- A 200 stream ending with finish_reason="error" is a failure.
			error({ message = "provider reported finish_reason error", kind = "server", usage = P.usage })
		end
		if not P.reasoning_field then
			for _, detail in ipairs(P.details or {}) do
				local shown = detail.text or detail.summary
				if type(shown) == "string" and shown ~= "" then
					P.reasoning_field = "reasoning_details"
					stream.stream_append(P, "thinking", shown)
				end
			end
		end
		if healer then
			emit_healed_events(healer:flush())
			if #P.calls == 0 and #healed_calls > 0 then
				for _, hc in ipairs(healed_calls) do
					P.calls[#P.calls + 1] = hc
				end
			end
		end
		local content = {}
		local details = nil
		for _, d in ipairs(P.details or {}) do
			details = details or {}
			details[#details + 1] = copy_detail(d)
		end
		if P.thinking ~= "" or details then
			local r_native = {}
			if P.reasoning_field ~= nil then
				r_native.reasoning_field = P.reasoning_field
			end
			if details ~= nil then
				r_native.reasoning_details = details
			end
			content[#content + 1] = {
				type = "reasoning",
				text = P.thinking,
				native = next(r_native) ~= nil and { [WIRE] = r_native } or nil,
			}
		end
		if P.text ~= "" then
			content[#content + 1] = { type = "text", text = P.text }
		end
		for _, c in ipairs(P.calls) do
			if c.name == "" then
				error({ message = "tool call without a name", kind = "malformed_response" })
			end
			local args, perr
			if c.healed_arguments ~= nil then
				args = c.healed_arguments
			elseif c.arguments_invalid then
				args, perr = nil, "arguments are not a JSON string"
			elseif c.arguments_present and c.args == "" then
				args = {}
			else
				args, perr = require("mare_lib").parse_args(c.args)
			end
			if not perr and healer_mod and type(args) == "table" then
				args = healer_mod.clean_arguments(heal_pattern, args)
			end
			local c_native = nil
			if type(c.signature) == "string" and c.signature ~= "" then
				c_native = {
					[WIRE] = {
						thought_signature = c.signature,
						signature_field = c.signature_field,
					},
				}
			end
			content[#content + 1] = {
				type = "tool_call",
				id = c.id,
				name = c.name,
				arguments = args,
				parse_error = perr,
				raw_arguments = perr and c.args or nil,
				native = c_native,
			}
		end
		local FILTERED_REASONS = {
			content_filter = true,
			sensitive = true,
			data_inspection_failed = true,
			DataInspectionFailed = true,
			censorship_blocked = true,
		}
		local NORMAL_REASONS = {
			stop = true,
			["end"] = true,
			tool_calls = true,
			function_call = true,
		}
		-- truncation wins over tool calls: an incomplete response never runs tools
		local msg = { role = "assistant", content = content, usage = P.usage or {} }
		if P.thought_signature then
			msg.native = { [WIRE] = { thought_signature = P.thought_signature } }
		end
		if P.finish == "length" then
			msg.stop_reason = "length"
		elseif FILTERED_REASONS[P.finish] then
			msg.stop_reason = "filtered"
			msg.raw_stop_reason = P.finish
			msg.error = ("the model output was filtered (%s)"):format(P.finish)
		elseif NORMAL_REASONS[P.finish] then
			msg.stop_reason = #P.calls > 0 and "tool_use" or "stop"
		else
			-- Unknown finish_reason must not silently become "stop".
			msg.stop_reason = "error"
			msg.raw_stop_reason = P.finish
			msg.error = "Provider finish_reason: " .. tostring(P.finish)
		end
		return msg
	end

	return P
end

return M
