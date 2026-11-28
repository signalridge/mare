-- provider-anthropic.wire: the Anthropic Messages wire format — history and tool conversion,
-- the SSE parser, stop reasons, error kinds and the overflow / image-limit wording; part of the
-- `provider-anthropic` plugin (plugins/provider-anthropic.lua), bound to the definition's `M`
-- at the start of its apply.

-- Anthropic's wording for too many / too large images in one request (Messages API limits)
local function IMAGE_LIMIT(msg)
	local m = string.lower(tostring(msg or ""))
	return m:find("too many images", 1, true) ~= nil
		or m:find("image exceeds", 1, true) ~= nil
		or m:find("image dimensions exceed", 1, true) ~= nil
		or m:find("many-image requests", 1, true) ~= nil
		or (m:find("maximum of", 1, true) ~= nil and m:find("images", 1, true) ~= nil)
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

local VERSION = "2023-06-01"
local WIRE = "anthropic-messages"

local M = {}

local function native_of(obj)
	return type(obj) == "table"
			and type(obj.native) == "table"
			and type(obj.native[WIRE]) == "table"
			and obj.native[WIRE]
		or nil
end

local function make_cache_applier(max_blocks)
	local used = 0
	return function(out_block, src_block)
		local na = native_of(src_block)
		if na and type(na.cache_control) == "table" and used < max_blocks then
			out_block.cache_control = table.copy(na.cache_control)
			used = used + 1
		end
		return out_block
	end, function()
		return used
	end
end

local function clean_id(id)
	return (string.gsub(id or "", "[^%w_%-]", "_"))
end

local ERROR_TOOL_IMAGE_PLACEHOLDER = "(image attached after the tool results)"

--- Replace contiguous image runs with a single placeholder.
local function replace_images_with_placeholder(content, placeholder)
	local result = {}
	local prev_was_placeholder = false
	for _, b in ipairs(content or {}) do
		if b.type == "image" then
			if not prev_was_placeholder then
				result[#result + 1] = { type = "text", text = placeholder }
			end
			prev_was_placeholder = true
		else
			result[#result + 1] = b
			prev_was_placeholder = (b.type == "text" and b.text == placeholder)
		end
	end
	return result
end

--- Split images out of an error tool result.
local function split_error_tool_result_images(msg, tool_call_id)
	if not msg.is_error then
		return nil
	end
	local images = {}
	for _, b in ipairs(msg.content or {}) do
		if b.type == "image" then
			images[#images + 1] = b
		end
	end
	if #images == 0 then
		return nil
	end
	local id = tool_call_id or msg.tool_call_id or ""
	local caption = msg.tool_name and ("Images from the error result of tool call %s (%s):"):format(id, msg.tool_name)
		or ("Images from the error result of tool call %s:"):format(id)
	return {
		content = replace_images_with_placeholder(msg.content, ERROR_TOOL_IMAGE_PLACEHOLDER),
		caption = caption,
		images = images,
	}
end

--- convert_messages(messages, no_cache, opts?) — opts.provider: the request's provider (thinking
--- and server blocks are replayed only to the provider that produced them; "anthropic" when
--- omitted); opts.replay_unsigned: thinking without a signature (opencode Go's gateway returns it
--- so) goes back as it came, but only within the current tool loop — the assistant messages after
--- the last user message (Anthropic keeps thinking only for the turn's tool use; an earlier
--- turn's is dropped, like CLIProxyAPI's `is-compat` keeps empty-signature blocks)
function M.convert_messages(messages, no_cache, opts)
	opts = opts or {}
	local provider = opts.provider or "anthropic"
	local max_cc = tonumber(opts.max_cache_breakpoints) or 4
	local block_cc_budget = no_cache and max_cc or math.max(0, max_cc - 1)
	local apply_block_cache_control, cc_used = make_cache_applier(block_cc_budget)
	local turn_start = 0
	if opts.replay_unsigned then
		for i, m in ipairs(messages) do
			if m.role == "user" then
				turn_start = i
			end
		end
	end
	local unreferenced
	if type(opts.deferred_tools) == "table" then
		unreferenced = {}
		for k, v in pairs(opts.deferred_tools) do
			if v then
				unreferenced[k] = true
			end
		end
	end
	local used_call_ids = {}
	for _, msg in ipairs(messages) do
		if type(msg.tool_call_id) == "string" and msg.tool_call_id ~= "" then
			used_call_ids[msg.tool_call_id] = true
			used_call_ids[clean_id(msg.tool_call_id)] = true
		end
		if type(msg.content) == "table" then
			for _, b in ipairs(msg.content) do
				if type(b) == "table" and b.type == "tool_call" and type(b.id) == "string" and b.id ~= "" then
					used_call_ids[b.id] = true
					used_call_ids[clean_id(b.id)] = true
				end
			end
		end
	end
	local function allocate_call_id(base)
		local id, seq = base, 1
		while used_call_ids[id] do
			seq = seq + 1
			id = base .. "_" .. seq
		end
		used_call_ids[id] = true
		return id
	end
	local out = {}
	local function push(role, blocks)
		if #blocks == 0 then
			return
		end
		local last = out[#out]
		-- merge consecutive same-role messages (e.g. several tool results)
		if last and last.role == role then
			for _, b in ipairs(blocks) do
				table.insert(last.content, b)
			end
		else
			out[#out + 1] = { role = role, content = blocks }
		end
	end
	local pending_displaced = {}
	local function flush_displaced()
		if #pending_displaced > 0 then
			push("user", pending_displaced)
			pending_displaced = {}
		end
	end
	local function has_more_tool_results(idx)
		for j = idx + 1, #messages do
			local r = messages[j].role
			if r == "tool_result" then
				return true
			elseif r == "assistant" then
				return false
			end
		end
		return false
	end
	for i, m in ipairs(messages) do
		if m.role == "user" then
			local blocks = {}
			for _, b in ipairs(m.content) do
				if b.type == "text" and b.text ~= "" then
					blocks[#blocks + 1] = apply_block_cache_control({ type = "text", text = b.text }, b)
				elseif b.type == "mention" then
					local label = b.name and ("@" .. b.path .. " (" .. b.name .. ")") or ("@" .. b.path)
					blocks[#blocks + 1] = apply_block_cache_control({ type = "text", text = label }, b)
				elseif b.type == "image" then
					blocks[#blocks + 1] = apply_block_cache_control({
						type = "image",
						source = { type = "base64", media_type = b.mime or "image/png", data = b.data },
					}, b)
				end
			end
			if has_more_tool_results(i) then
				-- Textified tool result inside a tool_result group: defer alongside displaced
				-- error images so every tool_result block precedes non-tool_result blocks.
				for _, b in ipairs(blocks) do
					pending_displaced[#pending_displaced + 1] = b
				end
			else
				flush_displaced()
				push("user", blocks)
				if
					m.compaction
					and unreferenced
					and #blocks > 0
					and type(m.details) == "table"
					and type(m.details.loaded_tools) == "table"
				then
					local refs, names = {}, {}
					for _, name in ipairs(m.details.loaded_tools) do
						if type(name) == "string" and unreferenced[name] then
							unreferenced[name] = nil
							refs[#refs + 1] = { type = "tool_reference", tool_name = name }
							names[#names + 1] = name
						end
					end
					if #refs > 0 then
						local call_id = allocate_call_id("compact_loaded_tools")
						push("assistant", {
							{
								type = "tool_use",
								id = call_id,
								name = "tool_search",
								input = { query = "select:" .. table.concat(names, ",") },
							},
						})
						push("user", {
							{
								type = "tool_result",
								tool_use_id = call_id,
								content = mare.json.array(refs),
								is_error = false,
							},
						})
					end
				end
			end
		elseif m.role == "assistant" then
			flush_displaced()
			local same = m.provider_id == nil or m.provider_id == provider
			local unsigned_ok = same and opts.replay_unsigned and i > turn_start
			local blocks = {}
			for _, b in ipairs(m.content) do
				local na = same and native_of(b) or nil
				if b.type == "reasoning" then
					if same and b.redacted then
						local data = na and na.redacted_data
						if type(data) == "string" and data ~= "" then
							blocks[#blocks + 1] =
								apply_block_cache_control({ type = "redacted_thinking", data = data }, b)
						end
					elseif na and type(na.signature) == "string" and na.signature ~= "" then
						blocks[#blocks + 1] = apply_block_cache_control(
							{ type = "thinking", thinking = b.text or "", signature = na.signature },
							b
						)
					elseif unsigned_ok and b.text and b.text ~= "" then
						blocks[#blocks + 1] =
							apply_block_cache_control({ type = "thinking", thinking = b.text, signature = "" }, b)
					end
				elseif b.type == "text" and b.text ~= "" then
					blocks[#blocks + 1] = apply_block_cache_control({ type = "text", text = b.text }, b)
				elseif b.type == "tool_call" then
					if b.hosted then
						if na and type(na.block) == "table" then
							blocks[#blocks + 1] = table.copy(na.block)
							if type(na.result_block) == "table" then
								blocks[#blocks + 1] = table.copy(na.result_block)
							end
						end
					else
						local input = b.arguments
						if type(input) ~= "table" or next(input) == nil then
							input = setmetatable({}, nil)
						end
						blocks[#blocks + 1] = apply_block_cache_control(
							{ type = "tool_use", id = clean_id(b.id), name = b.name, input = input },
							b
						)
					end
				elseif type(b.type) == "string" and b.type:sub(1, #WIRE + 1) == WIRE .. "." then
					-- server-side tool result or wire extension blocks go back verbatim when
					-- replaying to the same provider, e.g. when continuing a `pause_turn`
					if na and type(na.block) == "table" then
						blocks[#blocks + 1] = table.copy(na.block)
					end
				end
			end
			push("assistant", blocks)
		elseif m.role == "tool_result" then
			local tool_use_id = clean_id(m.tool_call_id)
			local split = split_error_tool_result_images(m, tool_use_id)
			local parts, has_image = {}, false
			for _, b in ipairs(split and split.content or m.content) do
				if b.type == "text" then
					parts[#parts + 1] = b.text
				elseif b.type == "image" then
					has_image = true
				end
			end
			local text = table.concat(parts, "\n")
			if not text:match("%S") then
				text = has_image and "(see attached image)" or "(no tool output)"
			end
			local refs = {}
			local loaded = not m.is_error
				and unreferenced
				and type(m.details) == "table"
				and type(m.details.loaded_tools) == "table"
				and m.details.loaded_tools
			if loaded then
				for _, name in ipairs(loaded) do
					if unreferenced[name] then
						unreferenced[name] = nil
						refs[#refs + 1] = { type = "tool_reference", tool_name = name }
					end
				end
			end
			if #refs > 0 then
				push("user", {
					{
						type = "tool_result",
						tool_use_id = tool_use_id,
						content = mare.json.array(refs),
						is_error = false,
					},
				})
				if text ~= "" then
					pending_displaced[#pending_displaced + 1] = { type = "text", text = text }
				end
			else
				local content_val = text
				if not split then
					local content_blocks, has_image, has_text = {}, false, false
					for _, b in ipairs(m.content) do
						if b.type == "text" and b.text ~= "" then
							has_text = true
							content_blocks[#content_blocks + 1] = { type = "text", text = b.text }
						elseif b.type == "image" then
							has_image = true
							content_blocks[#content_blocks + 1] = {
								type = "image",
								source = { type = "base64", media_type = b.mime or "image/png", data = b.data },
							}
						end
					end
					if has_image then
						if not has_text then
							table.insert(content_blocks, 1, { type = "text", text = "(see attached image)" })
						end
						content_val = mare.json.array(content_blocks)
					end
				end
				push("user", {
					{
						type = "tool_result",
						tool_use_id = tool_use_id,
						content = content_val,
						is_error = m.is_error or false,
					},
				})
				if split then
					pending_displaced[#pending_displaced + 1] = { type = "text", text = split.caption }
					for _, img in ipairs(split.images) do
						pending_displaced[#pending_displaced + 1] = {
							type = "image",
							source = { type = "base64", media_type = img.mime or "image/png", data = img.data },
						}
					end
				end
			end
		end
	end
	flush_displaced()
	-- prompt caching: breakpoint on the last block of the last user message (unless caller set one)
	for i = #out, 1, -1 do
		if no_cache or cc_used() >= max_cc then
			break
		end
		if out[i].role == "user" then
			local blocks = out[i].content
			if blocks[#blocks].cache_control == nil then
				local cc = opts.cache_control
				blocks[#blocks].cache_control = cc and { type = cc.type, ttl = cc.ttl } or { type = "ephemeral" }
			end
			break
		end
	end
	return out
end

function M.convert_tools(tools, dialect)
	local out = {}
	local sanitize = dialect and require("mare_lib.schema_sanitize").sanitize or nil
	for _, t in ipairs(tools or {}) do
		if t.type ~= "hosted" then
			local params = sanitize and sanitize(t.parameters, dialect) or t.parameters
			local entry = { name = t.name, description = t.description, input_schema = params }
			if t.defer_loading then
				entry.defer_loading = true
			end
			out[#out + 1] = entry
		end
	end
	return out
end

-- stop reasons: `pause_turn` → the turn continues as-is;
-- `refusal` → filtered, not retried; anything unknown is "other" (never drop the whole reply)
local STOPS = {
	end_turn = "stop",
	stop_sequence = "stop",
	tool_use = "tool_use",
	max_tokens = "length",
	model_context_window_exceeded = "length",
	pause_turn = "paused",
	refusal = "filtered",
	sensitive = "filtered",
}

local function malformed(what)
	-- typed stream: a malformed event is a protocol error, not retried
	error({ message = "Anthropic: malformed stream: " .. what, kind = "malformed_response" })
end

-- Anthropic error types → the shared kinds (HTTP errors and mid-stream `error` events alike)
local ERROR_KINDS = {
	overloaded_error = "overloaded",
	rate_limit_error = "rate_limit",
	api_error = "server",
	authentication_error = "auth",
	permission_error = "auth",
	invalid_request_error = "invalid",
}

--- new_parser(sink, on_error?, opts?) — `on_error(api_err)` turns a mid-stream `error` event into the
--- error raised (the provider passes llm:stream_error, the same mapping as HTTP errors).
function M.new_parser(sink, on_error, opts)
	local stream = require("mare_lib")
	local P = {
		blocks = {},
		hosted_calls = {},
		hosted_done = {},
		usage = { input = 0, output = 0, cache_read = 0, cache_write = 0 },
		usage_seq = 0,
		stop = nil,
		error = nil,
		done = false,
	}

	-- Cache-write usage. Every usage field an event carries is the cumulative value so far
	-- (Anthropic streaming docs: message_delta counts are cumulative); an absent field keeps
	-- its value. The total (`cache_creation_input_tokens`) and the 5m/1h breakdown
	-- (`cache_creation`) are tracked separately with the event that last set them:
	--   total and both buckets: equal sum -> exact; otherwise the parts disagree -> cost unknown;
	--   total and one bucket from the same event -> the other is the remainder (exact); from
	--   different events nobody can tell whether the total is still current -> cost unknown;
	--   total only -> all 5m (no breakdown reported); buckets only -> their sum.
	-- An unknown cost keeps the larger token count, so tokens are never under-reported.
	local function resolve_cache_write()
		local total, b5, b1 = P.cw_total, P.cw_5m, P.cw_1h
		local cw, long, unknown
		if total and b5 and b1 then
			local sum = b5.value + b1.value
			cw, long, unknown = math.max(total.value, sum), b1.value, sum ~= total.value
		elseif total and (b5 or b1) then
			local bucket = b5 or b1
			local exact = bucket.seq == total.seq and bucket.value <= total.value
			cw, unknown = math.max(total.value, bucket.value), not exact
			if b1 then
				long = b1.value
			elseif exact then
				long = total.value - b5.value
			end
		elseif total then
			cw = total.value
		elseif b5 or b1 then
			cw, long = (b5 and b5.value or 0) + (b1 and b1.value or 0), b1 and b1.value or nil
		else
			cw = 0
		end
		P.usage.cache_write, P.usage.cache_write_1h = cw, long
		P.usage.cost_unknown = unknown or nil
	end
	local function apply_cache_write(u)
		P.usage_seq = P.usage_seq + 1
		local seq = P.usage_seq
		if type(u.cache_creation_input_tokens) == "number" then
			P.cw_total = { value = u.cache_creation_input_tokens, seq = seq }
		end
		local cc = type(u.cache_creation) == "table" and u.cache_creation or nil
		if cc and type(cc.ephemeral_5m_input_tokens) == "number" then
			P.cw_5m = { value = cc.ephemeral_5m_input_tokens, seq = seq }
		end
		if cc and type(cc.ephemeral_1h_input_tokens) == "number" then
			P.cw_1h = { value = cc.ephemeral_1h_input_tokens, seq = seq }
		end
		resolve_cache_write()
	end

	local function finalize_block(b, emit_done)
		if not b then
			return
		end
		if b.type == "reasoning" and b._sig ~= nil then
			local sig = b._sig
			if type(sig) == "string" and sig ~= "" then
				b.native = b.native or {}
				b.native[WIRE] = b.native[WIRE] or {}
				b.native[WIRE].signature = sig
			end
			b._sig = nil
		elseif b._hosted_result_for then
			if b._raw_block then
				local call = b._hosted_result_for
				local raw = b._raw_block
				if b.json and b.json ~= "" then
					local decoded = mare.json.decode(b.json)
					if decoded ~= nil then
						raw.content = decoded
					end
				end
				call.native = call.native or {}
				call.native[WIRE] = call.native[WIRE] or {}
				call.native[WIRE].result_block = raw
				local is_error = raw.is_error == true
					or (
						type(raw.content) == "table"
						and type(raw.content.type) == "string"
						and raw.content.type:find("error", 1, true) ~= nil
					)
				local status = is_error and "failed" or "completed"
				call.result = { status = status, content = raw.content }
				b._raw_block, b.json, b.input_delta = nil, nil, nil
				if emit_done and call.id and not P.hosted_done[call.id] then
					P.hosted_done[call.id] = true
					sink({
						type = "hosted_tool_done",
						name = call.name or "server_tool",
						id = call.id,
						status = status,
						result = call.result,
					})
				end
			end
		elseif b.type == "tool_call" and b.hosted then
			if b._raw_block then
				local raw = b._raw_block
				if b.json and b.json ~= "" then
					local decoded = mare.json.decode(b.json)
					if decoded ~= nil then
						raw.input = decoded
					end
				end
				b.arguments = type(raw.input) == "table" and raw.input
					or (type(b.initial_input) == "table" and b.initial_input or {})
				b.native = b.native or {}
				b.native[WIRE] = b.native[WIRE] or {}
				b.native[WIRE].block = raw
				b._raw_block, b.json, b.initial_input, b.input_delta = nil, nil, nil, nil
			end
		elseif b.type == "tool_call" then
			if b.json ~= nil or b.initial_input ~= nil then
				local args, perr
				if b.json ~= "" then
					args, perr = require("mare_lib").parse_args(b.json)
				else
					args = b.initial_input
					if type(args) ~= "table" then
						perr = "arguments are not a JSON object"
					end
				end
				b.arguments, b.parse_error = args, perr
				b.raw_arguments = perr and b.input_delta and b.json or nil
				b.json, b.initial_input, b.input_delta = nil, nil, nil
			end
		elseif b._raw_block then
			local raw = b._raw_block
			if b.json and b.json ~= "" then
				raw.input = mare.json.decode(b.json) or raw.input
			end
			b.native = { [WIRE] = { block = raw } }
			b._raw_block, b.json, b.input_delta = nil, nil, nil
		end
	end

	function P.feed(ev)
		local d = mare.json.decode(ev.data)
		if type(d) ~= "table" then
			malformed("unparseable event data")
		end
		local t = d.type or ev.event
		if t == "message_start" then
			local u = d.message and d.message.usage or {}
			P.usage.input = u.input_tokens or 0
			P.usage.cache_read = u.cache_read_input_tokens or 0
			P.cw_total, P.cw_5m, P.cw_1h = nil, nil, nil
			apply_cache_write(u)
			P.usage.output = u.output_tokens or 0
		elseif t == "content_block_start" then
			local cb = d.content_block
			if type(cb) ~= "table" or type(cb.type) ~= "string" or cb.type == "" then
				malformed("content_block_start without content_block.type")
			end
			local b
			if cb.type == "text" then
				b = { type = "text", text = cb.text or "" }
			elseif cb.type == "thinking" then
				b = { type = "reasoning", text = cb.thinking or "", _sig = cb.signature or "" }
			elseif cb.type == "redacted_thinking" then
				b = {
					type = "reasoning",
					text = "",
					redacted = true,
					native = { [WIRE] = { redacted_data = cb.data } },
				}
			elseif cb.type == "tool_use" then
				-- blocks are typed here: a tool call without id or name is a protocol error
				-- , never guessed
				if type(cb.id) ~= "string" or cb.id == "" or type(cb.name) ~= "string" or cb.name == "" then
					malformed("tool_use block without id or name")
				end
				-- The pinned Anthropic Python SDK copies the initial content block and only
				-- overwrites its input when the accumulated JSON buffer is nonempty. This
				-- reference supports Anthropic behavior only.
				-- https://github.com/anthropics/anthropic-sdk-python/blob/5139cf2e7d07aa618a6c30b7e0c712f10ccb5ad9/src/anthropic/lib/streaming/_messages.py#L451-L475
				local original = mare.json.decode_lossless(ev.data).content_block.input
				b = { type = "tool_call", id = cb.id, name = cb.name, json = "", initial_input = original }
				sink({ type = "tool_call_start", tool_call_id = cb.id, name = cb.name })
			elseif cb.type == "server_tool_use" then
				local original = mare.json.decode_lossless(ev.data).content_block.input
				b = {
					type = "tool_call",
					id = cb.id,
					name = cb.name,
					hosted = true,
					json = "",
					initial_input = original,
					_raw_block = cb,
				}
				if type(cb.id) == "string" and cb.id ~= "" then
					P.hosted_calls[cb.id] = b
				end
				sink({
					type = "hosted_tool_start",
					name = cb.name or "server_tool",
					id = cb.id,
					status = "in_progress",
				})
			elseif cb.type then
				if type(cb.tool_use_id) == "string" and P.hosted_calls[cb.tool_use_id] ~= nil then
					-- server-side tool result attaches directly to its hosted tool_call
					b = { _hosted_result_for = P.hosted_calls[cb.tool_use_id], _raw_block = cb, json = "" }
				else
					-- other wire extension blocks: kept in native for same-wire replay
					b = { type = WIRE .. "." .. cb.type, _raw_block = cb, json = "" }
				end
			end
			P.blocks[(d.index or 0) + 1] = b and stream.stream_record(b, { "text", "_sig", "json" })
		elseif t == "content_block_delta" then
			local b = P.blocks[(d.index or 0) + 1]
			local delta = d.delta or {}
			if not b then
				malformed("delta before its content_block_start")
			end
			if delta.type == "text_delta" then
				stream.stream_append(b, "text", delta.text)
				sink({ type = "text_delta", delta = delta.text })
			elseif delta.type == "thinking_delta" then
				stream.stream_append(b, "text", delta.thinking)
				sink({ type = "reasoning_delta", delta = delta.thinking })
			elseif delta.type == "signature_delta" then
				stream.stream_append(b, "_sig", delta.signature, true)
			elseif delta.type == "input_json_delta" then
				b.input_delta = true
				stream.stream_append(b, "json", delta.partial_json, true)
				if b.type == "tool_call" and not b.hosted then
					sink({ type = "tool_call_delta", tool_call_id = b.id, delta = delta.partial_json })
				end
			end
		elseif t == "content_block_stop" then
			finalize_block(P.blocks[(d.index or 0) + 1], true)
		elseif t == "message_delta" then
			local sr = d.delta and d.delta.stop_reason
			if sr then
				P.stop = STOPS[sr] or "other"
				P.raw_stop = sr
			end
			local sd = d.delta and d.delta.stop_details
			if type(sd) == "table" and type(sd.explanation) == "string" and sd.explanation ~= "" then
				P.stop_explanation = sd.explanation
			end
			-- each field only when present: proxies omit input / cache counts here, and the
			-- values from message_start stand
			local u = type(d.usage) == "table" and d.usage or {}
			local function num(v)
				return type(v) == "number" and v or nil
			end
			local iter_input, iter_cache_read, iter_cache_write
			if type(u.iterations) == "table" and #u.iterations > 0 then
				local sum_in, sum_cr, sum_cw = 0, 0, 0
				local has_in, has_cr, has_cw = false, false, false
				for _, it in ipairs(u.iterations) do
					if type(it) == "table" then
						if num(it.input_tokens) then
							sum_in = sum_in + it.input_tokens
							has_in = true
						end
						if num(it.cache_read_input_tokens) then
							sum_cr = sum_cr + it.cache_read_input_tokens
							has_cr = true
						end
						if num(it.cache_creation_input_tokens) then
							sum_cw = sum_cw + it.cache_creation_input_tokens
							has_cw = true
						end
					end
				end
				if has_in then
					iter_input = sum_in
				end
				if has_cr then
					iter_cache_read = sum_cr
				end
				if has_cw then
					iter_cache_write = sum_cw
				end
			end
			P.usage.input = num(u.input_tokens) or iter_input or P.usage.input
			P.usage.output = num(u.output_tokens) or P.usage.output
			P.usage.cache_read = num(u.cache_read_input_tokens) or iter_cache_read or P.usage.cache_read
			if iter_cache_write and u.cache_creation_input_tokens == nil and type(u.cache_creation) ~= "table" then
				apply_cache_write({ cache_creation_input_tokens = iter_cache_write })
			else
				apply_cache_write(u)
			end
			local details = type(u.output_tokens_details) == "table" and u.output_tokens_details or {}
			P.usage.reasoning = num(details.thinking_tokens) or P.usage.reasoning
			if opts and opts.is_openrouter then
				local reported = stream.openrouter_cost(u)
				if reported ~= nil then
					P.usage.cost = reported
				end
			end
		elseif t == "message_stop" then
			P.done = true -- Anthropic's end signal
			return true
		elseif t == "error" then
			P.error = type(d.error) == "table" and d.error or { message = ev.data }
			return true
		end
		return false
	end

	function P.finish()
		if P.error then
			if on_error then
				error(on_error(P.error))
			end
			local et = P.error.type
			error({
				message = "Anthropic: " .. tostring(P.error.message or mare.json.encode(P.error)),
				kind = ERROR_KINDS[et],
				type = et,
			})
		end
		if not P.done then
			-- no message_stop: the stream was cut, a half answer is never a success — retryable
			error({ message = "Anthropic: stream closed before message_stop", kind = "stream_closed" })
		end
		local keys = {}
		for k in pairs(P.blocks) do
			keys[#keys + 1] = k
		end
		table.sort(keys)
		local content = {}
		for _, k in ipairs(keys) do
			local blk = P.blocks[k]
			finalize_block(blk, false)
			if not blk._hosted_result_for then
				local mat = stream.stream_materialize(blk)
				if mat.type == "tool_call" and mat.hosted then
					if mat.result == nil then
						mat.result = { status = P.raw_stop == "pause_turn" and "paused" or "incomplete" }
					end
					if mat.id and not P.hosted_done[mat.id] then
						P.hosted_done[mat.id] = true
						sink({
							type = "hosted_tool_done",
							name = mat.name or "server_tool",
							id = mat.id,
							status = mat.result.status,
							result = mat.result,
						})
					end
				end
				content[#content + 1] = mat
			end
		end
		local msg = { role = "assistant", content = content, usage = P.usage, stop_reason = P.stop or "stop" }
		if P.stop == "filtered" then
			-- show the refusal; there may be no text at all
			msg.error = P.stop_explanation or ("the model declined to answer (%s)"):format(P.raw_stop or "refusal")
			msg.raw_stop_reason = P.raw_stop
		elseif P.stop == "other" then
			msg.raw_stop_reason = P.raw_stop
		end
		return msg
	end

	return P
end

M.VERSION = VERSION
M.OVERFLOW = OVERFLOW
M.IMAGE_LIMIT = IMAGE_LIMIT
M.ERROR_KINDS = ERROR_KINDS

return M
