-- provider-openai.wire: the Responses wire format (input/tool conversion, compaction trim,
-- rate limits, error classification, the stream parser); pure functions, bound to the plugin
-- definition's `M` by apply (see the end of this file);
-- part of the `provider-openai` plugin (plugins/provider-openai.lua), loaded at the start of its apply.
local M = {}
local WIRE = "openai-responses"

local function native_of(obj)
	return type(obj) == "table"
			and type(obj.native) == "table"
			and type(obj.native[WIRE]) == "table"
			and obj.native[WIRE]
		or nil
end

--- Convert provider usage to mare's uncached input and separate cache counts.
function M.usage(u)
	u = u or {}
	local cached = u.input_tokens_details and u.input_tokens_details.cached_tokens or 0
	local written = u.input_tokens_details and u.input_tokens_details.cache_write_tokens or 0
	return {
		input = math.max(0, (u.input_tokens or 0) - cached - written),
		output = u.output_tokens or 0,
		cache_read = cached,
		cache_write = written,
		reasoning = (u.output_tokens_details and tonumber(u.output_tokens_details.reasoning_tokens))
			or tonumber(u.reasoning_tokens),
	}
end

--- Before /responses/compact, rewrite the newest tool outputs to a fixed note while the
--- estimate (instructions + items) exceeds the window; stops at the first item that isn't a tool
--- output.
local TRUNCATED_OUTPUT = "Output exceeded the available model context and was truncated"
function M.trim_outputs_to_window(input, instructions, model)
	local window = model and model.context_window
	if not window then
		return input
	end
	-- the shared estimate: an inline image is a constant, not its base64
	local function est(x)
		return mare.text.tokens(x)
	end
	local sizes, total = {}, est(instructions or "")
	for i, item in ipairs(input) do
		sizes[i] = est(item)
		total = total + sizes[i]
	end
	for i = #input, 1, -1 do
		if total <= window then
			break
		end
		local item = input[i]
		if item.type ~= "function_call_output" and item.type ~= "custom_tool_call_output" then
			break
		end
		local repl = table.copy(item)
		repl.output = TRUNCATED_OUTPUT
		total = total - sizes[i] + est(repl)
		input[i] = repl
	end
	return input
end

--- the main loop's output cap: min(model.max_output, what the window leaves after this request's
--- estimated input); the margin keeps room for an input counted higher than estimated; nil when
--- the model's max_output isn't known (missing, or only the provider's fallback: no cap, as
--- before). Sent so the gateway's own (undocumented) default never applies silently.
function M.output_cap(model, instructions, input, tools)
	local max, window = model and model.max_output, model and model.context_window
	if not max or model.max_output_default then
		return nil
	end
	if not window or window <= 0 then
		return max
	end
	local used = mare.text.tokens(instructions or "") + mare.text.tokens(input or {}) + mare.text.tokens(tools or {})
	local room = window - used - math.max(4096, math.ceil(used * 0.15))
	if room < 32768 then
		room = math.min(32768, window - used - 4096)
	end
	return math.min(max, math.max(1, room))
end

--- Of the server's compacted history, keep user / assistant messages and compaction items; drop
--- developer messages (stale instructions), reasoning, tool calls / outputs and anything unknown.
function M.keep_compacted_items(items)
	local out = {}
	for _, item in ipairs(items) do
		local t = item.type
		local keep
		if t == "message" or (t == nil and item.role) then
			keep = item.role == "user" or item.role == "assistant"
		else
			keep = t == "compaction" or t == "compaction_summary" or t == "context_compaction"
		end
		if keep then
			out[#out + 1] = item
		end
	end
	return out
end

--- Convert mare messages to Responses `input` items.
--- model_id: the request model. Encrypted reasoning is only replayed to the model that produced
--- it.
--- provider: the request provider ("openai" when omitted). A reasoning item is replayed to the
--- provider that produced it; callers set `opts.placeholder_reasoning = true` when the target
--- dialect needs an empty reasoning item prepended on tool turns without prior reasoning.
local function remote_items_valid(remote, model_id, provider)
	local expected_provider = type(provider) == "string" and provider or "openai"
	if type(remote) ~= "table" or remote.provider_id ~= expected_provider or type(remote.items) ~= "table" then
		return false
	end
	if type(remote.model_id) ~= "string" or type(model_id) ~= "string" or remote.model_id ~= model_id then
		return false
	end
	local has_compaction = false
	local enc_bytes = 0
	for _, item in ipairs(remote.items) do
		if type(item) == "table" and item.type == "compaction" then
			has_compaction = true
			if type(item.encrypted_content) == "string" then
				enc_bytes = enc_bytes + #item.encrypted_content
			end
		end
	end
	return has_compaction and enc_bytes <= 9500000
end

function M.convert_messages(messages, model_id, provider, opts)
	opts = opts or {}
	local placeholder_reasoning = opts.placeholder_reasoning == true
	local custom_ok = opts.custom ~= false
	local lookup = type(opts.lookup) == "function" and opts.lookup or nil
	local dialect = opts.dialect
	local sanitize = dialect and require("mare_lib.schema_sanitize").sanitize or nil
	local wire_tools_by_name = type(opts.wire_tools_by_name) == "table" and opts.wire_tools_by_name or nil
	local client_tool_search = opts.client_tool_search == true
	local heal_pattern = type(opts.markup_healing) == "string" and opts.markup_healing or nil
	local healer_mod = heal_pattern and require("provider-openai.dsml_strip") or nil
	local function deferred_wire_tool(name)
		local existing = wire_tools_by_name and wire_tools_by_name[name] or nil
		if type(existing) == "table" then
			local copy = table.copy(existing)
			copy.defer_loading = true
			return copy
		end
		local def = lookup and lookup(name) or nil
		if type(def) ~= "table" then
			return nil
		end
		local ff = custom_ok and def.freeform
		if ff then
			return {
				type = "custom",
				name = name,
				description = ff.description or (type(def.description) == "string" and def.description or ""),
				defer_loading = true,
				format = { type = "grammar", syntax = ff.syntax or "lark", definition = ff.definition },
			}
		end
		local raw_params = type(def.parameters) == "table" and def.parameters or { type = "object", properties = {} }
		local params = sanitize and sanitize(raw_params, dialect) or raw_params
		return {
			type = "function",
			name = name,
			description = type(def.description) == "string" and def.description or "",
			parameters = params,
			strict = false,
			defer_loading = true,
		}
	end
	local input = {}
	for _, m in ipairs(messages) do
		if m.compaction and remote_items_valid(m.remote, model_id, provider) then
			-- remote compaction: the server's compacted history replaces
			-- the readable placeholder; developer items are dropped (stale instructions
			-- should_keep_compacted_history_item)
			for _, item in ipairs(m.remote.items) do
				if not (item.type == "message" and item.role == "developer") then
					input[#input + 1] = item
				end
			end
		elseif m.role == "user" then
			local content = {}
			for _, b in ipairs(m.content) do
				if b.type == "text" then
					content[#content + 1] = { type = "input_text", text = b.text }
				elseif b.type == "mention" then
					local label = b.name and ("@" .. b.path .. " (" .. b.name .. ")") or ("@" .. b.path)
					content[#content + 1] = { type = "input_text", text = label }
				elseif b.type == "image" then
					content[#content + 1] = {
						type = "input_image",
						image_url = ("data:%s;base64,%s"):format(b.mime or "image/png", b.data),
						detail = b.detail or "auto",
					}
				end
			end
			input[#input + 1] = { role = "user", content = content }
		elseif m.role == "assistant" then
			local same_provider = (m.provider_id == nil or m.provider_id == (provider or "openai"))
				and (model_id == nil or m.model_id == nil or m.model_id == model_id)
			local turn_items, has_reasoning, has_call = {}, false, false
			for _, b in ipairs(m.content) do
				local nr = same_provider and native_of(b) or nil
				if b.type == "reasoning" then
					if nr and type(nr.item) == "table" then
						has_reasoning = true
						local r_item = table.copy(nr.item)
						if healer_mod then
							r_item = healer_mod.clean_responses_item(heal_pattern, r_item)
						end
						turn_items[#turn_items + 1] = r_item
					end
				elseif b.type == "text" then
					local text_val = healer_mod and healer_mod.strip_leaked_text(heal_pattern, b.text) or b.text
					if not healer_mod or text_val ~= "" then
						local annotations = (nr and type(nr.annotations) == "table") and table.copy(nr.annotations)
							or mare.json.array()
						turn_items[#turn_items + 1] = {
							type = "message",
							role = "assistant",
							content = { { type = "output_text", text = text_val, annotations = annotations } },
						}
					end
				elseif b.type == "tool_call" then
					if b.hosted then
						if nr and type(nr.item) == "table" then
							turn_items[#turn_items + 1] = table.copy(nr.item)
							if type(nr.output_item) == "table" then
								turn_items[#turn_items + 1] = table.copy(nr.output_item)
							end
						end
					elseif
						(nr and type(nr.item) == "table" and nr.item.type == "tool_search_call")
						or (client_tool_search and b.name == "tool_search")
					then
						has_call = true
						local raw_item = nr and type(nr.item) == "table" and nr.item or nil
						local tsc = {
							type = "tool_search_call",
							call_id = b.id,
							execution = (raw_item and type(raw_item.execution) == "string") and raw_item.execution
								or "client",
							arguments = type(b.arguments) == "table" and b.arguments or {},
						}
						if raw_item and type(raw_item.id) == "string" and raw_item.id ~= "" then
							tsc.id = raw_item.id
						end
						if raw_item and type(raw_item.status) == "string" and raw_item.status ~= "" then
							tsc.status = raw_item.status
						end
						turn_items[#turn_items + 1] = tsc
					else
						local def = custom_ok and lookup and lookup(b.name) or nil
						has_call = true
						if def and def.freeform then
							-- freeform (custom) tool call: raw input, no JSON
							turn_items[#turn_items + 1] = {
								type = "custom_tool_call",
								call_id = b.id,
								name = b.name,
								input = (b.arguments or {}).input or "",
								status = "completed",
							}
						else
							local fc_args = b.arguments or {}
							if healer_mod and not b.raw_arguments then
								fc_args = healer_mod.clean_arguments(heal_pattern, fc_args)
							end
							turn_items[#turn_items + 1] = {
								type = "function_call",
								call_id = b.id,
								name = b.name,
								arguments = b.raw_arguments or mare.json.encode(fc_args),
								status = "completed",
							}
						end
					end
				elseif type(b.type) == "string" and b.type:sub(1, #WIRE + 1) == WIRE .. "." then
					if nr and type(nr.item) == "table" then
						turn_items[#turn_items + 1] = table.copy(nr.item)
					end
				end
			end
			if placeholder_reasoning and not has_reasoning and (has_call or opts.has_tools) then
				input[#input + 1] = {
					type = "reasoning",
					summary = mare.json.array(),
					content = { { type = "reasoning_text", text = " " } },
				}
			end
			for _, it in ipairs(turn_items) do
				input[#input + 1] = it
			end
		elseif m.role == "tool_result" then
			local parts, items, has_image = {}, {}, false
			for _, b in ipairs(m.content) do
				if b.type == "text" then
					parts[#parts + 1] = b.text
					items[#items + 1] = { type = "input_text", text = b.text }
				elseif b.type == "image" then
					has_image = true
					items[#items + 1] = {
						type = "input_image",
						image_url = ("data:%s;base64,%s"):format(b.mime or "image/png", b.data),
						detail = b.detail or "auto",
					}
				end
			end
			local text_output = table.concat(parts, "\n")
			if not has_image and not text_output:match("%S") then
				text_output = "(no tool output)"
			end
			local search_tools
			if not m.is_error and type(m.details) == "table" and type(m.details.loaded_tools) == "table" then
				search_tools = {}
				local seen = {}
				for _, name in ipairs(m.details.loaded_tools) do
					if type(name) == "string" and not seen[name] then
						local entry = deferred_wire_tool(name)
						if entry then
							seen[name] = true
							search_tools[#search_tools + 1] = entry
						end
					end
				end
			end
			input[#input + 1] = {
				type = "function_call_output", -- retyped below for custom / tool_search calls
				call_id = m.tool_call_id,
				-- images go back as content items; text-only outputs stay a plain string
				output = has_image and items or text_output,
				text_output = text_output,
				_search_tools = search_tools,
			}
		end
	end
	return M.normalize(input)
end

local CALL_TYPES = {
	function_call = "function_call_output",
	custom_tool_call = "custom_tool_call_output",
	tool_search_call = "tool_search_output",
}
local OUTPUT_TYPES = {
	function_call_output = true,
	custom_tool_call_output = true,
	tool_search_output = true,
}
-- server-issued item id prefixes that may be sent back (non-server ids are stripped)
local SERVER_ID_PREFIXES = { "rs_", "msg_", "fc_", "ctc_", "ws_", "tsc_", "tso_" }

--- Every call gets an output (missing → "aborted" or empty tool_search_output), outputs without
--- a matching call are dropped, outputs take the call's kind, and non-server item ids are stripped.
function M.normalize(items)
	local calls, outputs = {}, {}
	for _, it in ipairs(items) do
		local cid = type(it.call_id) == "string" and it.call_id ~= "" and it.call_id or nil
		if cid and it.execution ~= "server" then
			if CALL_TYPES[it.type] then
				calls[cid] = it.type
			elseif OUTPUT_TYPES[it.type] then
				outputs[cid] = true
			end
		end
	end
	local out = {}
	for _, it in ipairs(items) do
		if it.id ~= nil then
			local keep = false
			if type(it.id) == "string" then
				for _, pre in ipairs(SERVER_ID_PREFIXES) do
					if string.sub(it.id, 1, #pre) == pre then
						keep = true
					end
				end
			end
			if not keep then
				it.id = nil
			end
		end
		if OUTPUT_TYPES[it.type] and it.execution ~= "server" then
			local cid = type(it.call_id) == "string" and it.call_id ~= "" and it.call_id or nil
			local kind = cid and calls[cid] or nil
			if kind == "tool_search_call" then
				local tools = type(it.tools) == "table" and it.tools
					or (type(it._search_tools) == "table" and it._search_tools or {})
				out[#out + 1] = {
					type = "tool_search_output",
					id = it.id,
					call_id = cid,
					execution = type(it.execution) == "string" and it.execution or "client",
					status = type(it.status) == "string" and it.status or "completed",
					tools = mare.json.array(tools),
				}
			elseif kind then
				it.type = CALL_TYPES[kind]
				-- custom (freeform) tool outputs are plain text
				if it.type == "custom_tool_call_output" and type(it.output) == "table" then
					it.output = it.text_output or ""
				end
				it.text_output = nil
				it._search_tools = nil
				out[#out + 1] = it
			end -- orphan output: dropped
		else
			out[#out + 1] = it
			local cid = type(it.call_id) == "string" and it.call_id ~= "" and it.call_id or nil
			if cid and CALL_TYPES[it.type] and it.execution ~= "server" and not outputs[cid] then
				if it.type == "tool_search_call" then
					out[#out + 1] = {
						type = "tool_search_output",
						call_id = cid,
						execution = type(it.execution) == "string" and it.execution or "client",
						status = "completed",
						tools = mare.json.array(),
					}
				else
					out[#out + 1] = { type = CALL_TYPES[it.type], call_id = cid, output = "aborted" }
				end
			end
		end
	end
	return out
end

--- Build an OpenAI hosted `web_search` tool declaration from mode + optional policy config
--- . Returns nil when disabled.
function M.hosted_web_search_tool(mode, ws_cfg)
	if mode == nil or mode == "disabled" then
		return nil
	end
	local external_web_access, indexed_web_access
	if mode == "cached" then
		external_web_access = false
	elseif mode == "indexed" then
		external_web_access, indexed_web_access = true, true
	elseif mode == "live" then
		external_web_access = true
	else
		error("provider-openai: invalid web_search mode: " .. tostring(mode))
	end
	local tool = {
		type = "web_search",
		external_web_access = external_web_access,
	}
	if indexed_web_access ~= nil then
		tool.indexed_web_access = indexed_web_access
	end
	if type(ws_cfg) == "table" then
		if type(ws_cfg.filters) == "table" then
			tool.filters = table.copy(ws_cfg.filters)
		end
		if type(ws_cfg.user_location) == "table" then
			local loc = table.copy(ws_cfg.user_location)
			loc.type = loc.type or "approximate"
			tool.user_location = loc
		end
		if ws_cfg.search_context_size ~= nil then
			local scs = ws_cfg.search_context_size
			assert(
				scs == "low" or scs == "medium" or scs == "high",
				"provider-openai: search_context_size must be low, medium or high"
			)
			tool.search_context_size = scs
		end
		if type(ws_cfg.search_content_types) == "table" then
			tool.search_content_types = table.copy(ws_cfg.search_content_types)
		end
	end
	return tool
end

--- lookup(name) -> tool definition (for its `freeform` grammar, if any). Responses supports
--- custom tools, so grammar tools are declared as `type = "custom"`.
--- opts.custom = false: the endpoint rejects custom tools (opencode Go: "custom tools are not
--- supported on this endpoint") — declare every tool as a function tool; freeform tools also
--- define JSON `parameters` (apply_patch: `{input}`).
function M.convert_tools(tools, lookup, opts)
	local custom_ok = not (opts and opts.custom == false)
	local dialect = opts and opts.dialect or nil
	local sanitize = dialect and require("mare_lib.schema_sanitize").sanitize or nil
	local allow_hosted_ws = not (opts and opts.web_search_supported == false)
	local default_ws_mode = opts and opts.web_search_default_mode or nil
	local default_ws_cfg = opts and opts.web_search_default_config or nil
	local allow_hosted = not (opts and opts.allow_hosted == false)
	local has_deferred = false
	for _, t in ipairs(tools or {}) do
		if t.type ~= "hosted" and t.defer_loading then
			has_deferred = true
			break
		end
	end
	local use_client_tool_search = has_deferred or (opts and opts.defer_loading == true)
	local out = {}
	local by_name = {}
	local explicit_ws = false
	local has_client_tool_search = false
	for _, t in ipairs(tools or {}) do
		if t.type == "hosted" then
			if t.name == "web_search" then
				explicit_ws = true
				if allow_hosted and allow_hosted_ws then
					local wopts = type(t.options) == "table" and t.options or {}
					local mode = wopts.mode or default_ws_mode or "live"
					if mode ~= "disabled" then
						local merged_cfg
						if type(default_ws_cfg) == "table" or next(wopts) ~= nil then
							merged_cfg = {}
							if type(default_ws_cfg) == "table" then
								for k, v in pairs(default_ws_cfg) do
									merged_cfg[k] = v
								end
							end
							for k, v in pairs(wopts) do
								if k ~= "mode" then
									merged_cfg[k] = v
								end
							end
						end
						local ws_tool = M.hosted_web_search_tool(mode, merged_cfg)
						if ws_tool then
							out[#out + 1] = ws_tool
						end
					end
				end
			end
		elseif use_client_tool_search and t.name == "tool_search" then
			local params = sanitize and sanitize(t.parameters, dialect) or t.parameters
			has_client_tool_search = true
			out[#out + 1] = {
				type = "tool_search",
				execution = "client",
				description = t.description,
				parameters = params,
			}
		else
			local def = lookup and lookup(t.name)
			local ff = custom_ok and def and def.freeform
			local entry
			if ff then
				entry = {
					type = "custom",
					name = t.name,
					description = ff.description or t.description,
					format = { type = "grammar", syntax = ff.syntax or "lark", definition = ff.definition },
				}
			else
				local params = sanitize and sanitize(t.parameters, dialect) or t.parameters
				entry = {
					type = "function",
					name = t.name,
					description = t.description,
					parameters = params,
					strict = false,
				}
			end
			if t.defer_loading then
				entry.defer_loading = true
			end
			by_name[t.name] = entry
			out[#out + 1] = entry
		end
	end
	return out, explicit_ws, by_name, has_client_tool_search
end

--- Codex codex-api/src/rate_limits.rs: x-codex-{primary,secondary}-{used-percent,window-minutes,
--- reset-at}, credits, limit-name, rate-limit-reached-type.
function M.parse_rate_limits(h)
	local function num(k)
		return tonumber(h[k] or "")
	end
	local function window(which)
		local used = num(("x-codex-%s-used-percent"):format(which))
		if not used then
			return nil
		end
		return {
			used_percent = used,
			window_minutes = num(("x-codex-%s-window-minutes"):format(which)),
			reset_at_ms = require("mare_lib").to_epoch_ms(num(("x-codex-%s-reset-at"):format(which)), "s"),
		}
	end
	local snap = {
		primary = window("primary"),
		secondary = window("secondary"),
		credits = h["x-codex-credits-balance"] or h["x-codex-credits"],
		has_credits = h["x-codex-credits-has-credits"],
		unlimited = h["x-codex-credits-unlimited"],
		limit_name = h["x-codex-limit-name"],
		reached = h["x-codex-rate-limit-reached-type"],
	}
	if not snap.primary and not snap.secondary and not snap.reached and not snap.credits then
		return nil
	end
	return snap
end

M.ERRORS = require("provider-openai.errors")

local TEXT_PART_SEP = "\n\n"

local function count_leading_newlines(s)
	local i = 1
	while i <= #s and s:byte(i) == 10 do
		i = i + 1
	end
	return i - 1
end

local function count_trailing_newlines(s)
	local i = #s
	while i >= 1 and s:byte(i) == 10 do
		i = i - 1
	end
	return #s - i
end

local function append_non_empty_part(parts, text, trailing_nl)
	if type(text) ~= "string" or text == "" then
		return 0, trailing_nl
	end
	local sep_len = 0
	if #parts > 0 then
		local missing = math.max(0, #TEXT_PART_SEP - (trailing_nl + count_leading_newlines(text)))
		if missing > 0 then
			parts[#parts + 1] = string.rep("\n", missing)
			sep_len = missing
		end
	end
	parts[#parts + 1] = text
	local text_trailing = count_trailing_newlines(text)
	if text_trailing == #text then
		return sep_len, trailing_nl + sep_len + #text
	end
	return sep_len, text_trailing
end

--- Streaming state machine: feed SSE events, get the final assistant message.
function M.new_parser(sink, on_error, opts)
	opts = opts or {}
	local stream = require("mare_lib")
	local heal_pattern = type(opts.markup_healing) == "string" and opts.markup_healing or nil
	local healer_mod = heal_pattern and require("provider-openai.dsml_strip") or nil
	local AMBIGUOUS_INDEX = {}
	local P = {
		content = {},
		tools = {},
		tools_by_call_id = {},
		tools_by_index = {},
		item_key_by_id = {},
		item_key_by_index = {},
		pending_tool_search = {},
		tool_search_id_by_index = {},
		tool_search_seq = 0,
		usage = nil,
		stop = "stop",
		text_idx = {},
		error = nil,
	}
	local function tool_search_block_id(item, output_index)
		local explicit = (type(item.id) == "string" and item.id ~= "" and item.id)
			or (type(item.call_id) == "string" and item.call_id ~= "" and item.call_id)
			or nil
		if explicit then
			if output_index ~= nil then
				P.tool_search_id_by_index[output_index] = explicit
			end
			return explicit
		end
		if output_index ~= nil and P.tool_search_id_by_index[output_index] then
			return P.tool_search_id_by_index[output_index]
		end
		P.tool_search_seq = P.tool_search_seq + 1
		local synth = "tool_search_" .. tostring(P.tool_search_seq)
		if output_index ~= nil then
			P.tool_search_id_by_index[output_index] = synth
		end
		return synth
	end
	local function extract_tool_search_names(tools_list)
		local names = {}
		if type(tools_list) ~= "table" then
			return names
		end
		for _, entry in ipairs(tools_list) do
			if type(entry) == "table" then
				if entry.type == "namespace" and type(entry.tools) == "table" then
					for _, sub in ipairs(entry.tools) do
						if type(sub) == "table" and type(sub.name) == "string" then
							names[#names + 1] = sub.name
						end
					end
				elseif type(entry.name) == "string" then
					names[#names + 1] = entry.name
				end
			end
		end
		return names
	end
	local has_text_part, need_part_sep, in_newline_only_boundary, last_text_key, trailing_nl =
		false, false, false, nil, 0

	local function canonical_item_key(item_id, output_index)
		local id = type(item_id) == "string" and item_id ~= "" and item_id or nil
		local canon = (output_index ~= nil and P.item_key_by_index[output_index])
			or (id and P.item_key_by_id[id])
			or (output_index ~= nil and ("idx:" .. tostring(output_index)))
			or ("id:" .. tostring(id or ""))
		if output_index ~= nil then
			P.item_key_by_index[output_index] = canon
		end
		if id then
			P.item_key_by_id[id] = canon
		end
		return canon
	end

	local function flush_boundary_sep()
		if in_newline_only_boundary and trailing_nl < #TEXT_PART_SEP then
			sink({ type = "text_delta", delta = string.rep("\n", #TEXT_PART_SEP - trailing_nl) })
			trailing_nl = #TEXT_PART_SEP
		end
		in_newline_only_boundary = false
	end

	local function emit_text_delta(d)
		local delta = d.delta
		if type(delta) ~= "string" or delta == "" then
			if delta ~= nil then
				sink({ type = "text_delta", delta = delta })
			end
			return
		end
		local item_key = canonical_item_key(d.item_id, d.output_index)
		local key = item_key .. ":" .. tostring(d.content_index or 0)
		if last_text_key ~= nil and last_text_key ~= key then
			flush_boundary_sep()
		end
		local at_boundary = has_text_part and (need_part_sep or (last_text_key ~= nil and last_text_key ~= key))
		if at_boundary then
			local leading_nl = count_leading_newlines(delta)
			if leading_nl == #delta then
				trailing_nl = trailing_nl + #delta
				need_part_sep = trailing_nl < #TEXT_PART_SEP
				in_newline_only_boundary = need_part_sep
			else
				local missing = math.max(0, #TEXT_PART_SEP - (trailing_nl + leading_nl))
				if missing > 0 then
					sink({ type = "text_delta", delta = string.rep("\n", missing) })
				end
				need_part_sep = false
				in_newline_only_boundary = false
				trailing_nl = count_trailing_newlines(delta)
			end
		else
			in_newline_only_boundary = false
			local delta_trailing = count_trailing_newlines(delta)
			if delta_trailing == #delta then
				trailing_nl = trailing_nl + #delta
			else
				trailing_nl = delta_trailing
			end
		end
		has_text_part = true
		last_text_key = key
		sink({ type = "text_delta", delta = delta })
	end

	local function non_empty_string(v)
		return type(v) == "string" and v:match("%S") ~= nil and v or nil
	end

	local function malformed_tool_call(item, reason)
		error({
			message = ("OpenAI: malformed %s item: %s"):format(tostring(item.type), reason),
			kind = "malformed_response",
		})
	end

	local function resolve_tool_call_identity(item, output_index)
		local item_id = non_empty_string(item.id)
		local raw_call_id = non_empty_string(item.call_id)
		local raw_name = non_empty_string(item.name)
		local tc
		if item_id or raw_call_id then
			local by_item = item_id and P.tools[item_id] or nil
			local by_call = raw_call_id and P.tools_by_call_id[raw_call_id] or nil
			if by_item and by_call and by_item ~= by_call then
				malformed_tool_call(item, "conflicting item.id and call_id")
			end
			tc = by_item or by_call
		elseif output_index ~= nil then
			local by_idx = P.tools_by_index[output_index]
			if by_idx == AMBIGUOUS_INDEX then
				malformed_tool_call(item, "ambiguous output_index")
			end
			if by_idx and not by_idx.done then
				tc = by_idx
			end
		end
		if tc then
			if tc.item_type and item.type and tc.item_type ~= item.type then
				malformed_tool_call(item, "conflicting item type")
			end
			if item_id and tc.item_id and tc.item_id ~= item_id then
				malformed_tool_call(item, "conflicting item.id")
			end
			if raw_call_id and tc.id and tc.id ~= raw_call_id then
				malformed_tool_call(item, "conflicting call_id")
			end
			if raw_name and tc.name and tc.name ~= raw_name then
				malformed_tool_call(item, "conflicting tool name")
			end
		end
		local name = raw_name or (tc and non_empty_string(tc.name))
		local call_id = raw_call_id or (tc and non_empty_string(tc.id))
		if not name or not call_id then
			local what = (not name and not call_id) and "tool name and call_id"
				or (not name and "tool name")
				or "call_id"
			malformed_tool_call(item, "missing " .. what)
		end
		if tc then
			tc.done = true
		end
		return call_id, name
	end

	function P.feed(ev)
		local data = ev.data or ""
		if data == "[DONE]" then
			return true
		end
		if not data:match("%S") then
			return false
		end
		local d = mare.json.decode(data)
		if type(d) ~= "table" then
			-- typed stream: an unparseable event is a protocol error, not retried
			error({ message = "OpenAI: malformed stream event", kind = "malformed_response" })
		end
		local t = d.type or ev.event
		if t == "response.output_item.added" then
			local item = d.item or {}
			canonical_item_key(item.id, d.output_index)
			if item.type == "message" then
				flush_boundary_sep()
				has_text_part, need_part_sep, in_newline_only_boundary, last_text_key, trailing_nl =
					false, false, false, nil, 0
			elseif item.type == "function_call" or item.type == "custom_tool_call" then
				local item_id = non_empty_string(item.id)
				local call_id = non_empty_string(item.call_id)
				local name = non_empty_string(item.name)
				local rec = stream.stream_record(
					{ item_id = item_id, id = call_id, name = name, item_type = item.type, args = "" },
					{ "args" }
				)
				if item_id then
					P.tools[item_id] = rec
				end
				if call_id then
					P.tools_by_call_id[call_id] = rec
				end
				if d.output_index ~= nil then
					local existing = P.tools_by_index[d.output_index]
					if existing == nil then
						P.tools_by_index[d.output_index] = rec
					elseif existing ~= rec then
						P.tools_by_index[d.output_index] = AMBIGUOUS_INDEX
					end
				end
				if name then
					sink({ type = "tool_call_start", tool_call_id = call_id, name = name })
				end
			elseif item.type == "web_search_call" then
				sink({
					type = "hosted_tool_start",
					name = "web_search",
					id = item.id,
					status = item.status,
					action = item.action,
				})
			elseif item.type == "tool_search_call" then
				if item.execution == "server" then
					sink({
						type = "hosted_tool_start",
						name = "tool_search",
						id = tool_search_block_id(item, d.output_index),
						status = item.status,
					})
				else
					sink({
						type = "tool_call_start",
						tool_call_id = item.call_id,
						name = "tool_search",
					})
				end
			end
		elseif
			t == "response.web_search_call.in_progress"
			or t == "response.web_search_call.searching"
			or t == "response.web_search_call.completed"
		then
			sink({
				type = "hosted_tool_update",
				name = "web_search",
				id = d.item_id,
				status = string.sub(t, #"response.web_search_call." + 1),
			})
		elseif
			t == "response.tool_search_call.in_progress"
			or t == "response.tool_search_call.searching"
			or t == "response.tool_search_call.completed"
		then
			sink({
				type = "hosted_tool_update",
				name = "tool_search",
				id = d.item_id,
				status = string.sub(t, #"response.tool_search_call." + 1),
			})
		elseif t == "response.output_text.annotation.added" then
			if type(d.annotation) == "table" then
				sink({
					type = "annotation",
					id = d.item_id,
					content_index = d.content_index,
					annotation_index = d.annotation_index,
					annotation = d.annotation,
				})
			end
		elseif t == "response.output_text.delta" then
			emit_text_delta(d)
		elseif t == "response.content_part.done" then
			local part_type = type(d.part) == "table" and d.part.type or nil
			if has_text_part and (part_type == nil or part_type == "output_text" or part_type == "refusal") then
				flush_boundary_sep()
				need_part_sep = true
			end
		elseif t == "response.reasoning_summary_text.delta" or t == "response.reasoning_text.delta" then
			sink({ type = "reasoning_delta", delta = d.delta })
		elseif t == "response.reasoning_summary_part.done" then
			sink({ type = "reasoning_delta", delta = "\n\n" })
		elseif t == "response.refusal.delta" then
			emit_text_delta(d)
		elseif t == "response.function_call_arguments.delta" or t == "response.custom_tool_call_input.delta" then
			local item_id = non_empty_string(d.item_id)
			local tc
			if item_id then
				tc = P.tools[item_id]
			elseif d.output_index ~= nil then
				local by_idx = P.tools_by_index[d.output_index]
				if by_idx and by_idx ~= AMBIGUOUS_INDEX then
					tc = by_idx
				end
			end
			if tc then
				stream.stream_append(tc, "args", d.delta or "")
				sink({ type = "tool_call_delta", tool_call_id = tc.id, delta = d.delta })
			end
		elseif t == "response.output_item.done" then
			local item = d.item or {}
			if item.type == "reasoning" then
				local parts = {}
				for _, s in ipairs(item.summary or {}) do
					parts[#parts + 1] = s.text
				end
				table.insert(P.content, {
					type = "reasoning",
					text = table.concat(parts, "\n\n"),
					native = { [WIRE] = { item = item } },
				})
			elseif item.type == "message" then
				flush_boundary_sep()
				has_text_part, need_part_sep, in_newline_only_boundary, last_text_key, trailing_nl =
					false, false, false, nil, 0
				local parts, citations, annotations, offset, part_trailing_nl = {}, {}, {}, 0, 0
				for _, c in ipairs(item.content or {}) do
					if c.type == "output_text" then
						local t = c.text or ""
						local sep_len, next_trailing_nl = append_non_empty_part(parts, t, part_trailing_nl)
						offset = offset + sep_len
						part_trailing_nl = next_trailing_nl
						if type(c.annotations) == "table" then
							for _, ann in ipairs(c.annotations) do
								if type(ann) == "table" then
									local copy = table.copy(ann)
									if type(copy.start_index) == "number" then
										copy.start_index = offset + copy.start_index
									end
									if type(copy.end_index) == "number" then
										copy.end_index = offset + copy.end_index
									end
									annotations[#annotations + 1] = copy
									if copy.type == "url_citation" or type(copy.url) == "string" then
										citations[#citations + 1] = {
											type = copy.type or "url_citation",
											url = copy.url,
											title = copy.title,
											start_index = copy.start_index,
											end_index = copy.end_index,
										}
									end
								end
							end
						end
						offset = offset + #t
					elseif c.type == "refusal" then
						local r = c.refusal or ""
						local sep_len, next_trailing_nl = append_non_empty_part(parts, r, part_trailing_nl)
						offset = offset + sep_len + #r
						part_trailing_nl = next_trailing_nl
					end
				end
				local block = { type = "text", text = table.concat(parts) }
				if #annotations > 0 then
					block.native = { [WIRE] = { annotations = annotations } }
				end
				if #citations > 0 then
					block.citations = citations
				end
				table.insert(P.content, block)
			elseif item.type == "web_search_call" then
				table.insert(P.content, {
					type = "tool_call",
					id = item.id,
					name = "web_search",
					arguments = type(item.action) == "table" and item.action or {},
					hosted = true,
					result = { status = item.status },
					native = { [WIRE] = { item = item } },
				})
				sink({
					type = "hosted_tool_done",
					name = "web_search",
					id = item.id,
					status = item.status,
					action = item.action,
				})
			elseif item.type == "tool_search_call" then
				if item.execution == "server" then
					local bid = tool_search_block_id(item, d.output_index)
					local block = {
						type = "tool_call",
						id = bid,
						name = "tool_search",
						arguments = type(item.arguments) == "table" and item.arguments or {},
						hosted = true,
						result = { status = item.status or "completed" },
						native = { [WIRE] = { item = item } },
					}
					table.insert(P.content, block)
					P.pending_tool_search[#P.pending_tool_search + 1] = {
						block = block,
						call_id = type(item.call_id) == "string" and item.call_id ~= "" and item.call_id or nil,
					}
				else
					local args, perr
					if type(item.arguments) == "table" then
						args = item.arguments
					elseif type(item.arguments) == "string" then
						args, perr = require("mare_lib").parse_args(item.arguments)
					else
						args = {}
					end
					table.insert(P.content, {
						type = "tool_call",
						id = item.call_id,
						name = "tool_search",
						arguments = args,
						parse_error = perr,
						native = { [WIRE] = { item = item } },
					})
					P.stop = "tool_use"
				end
			elseif item.type == "tool_search_output" then
				local cid = type(item.call_id) == "string" and item.call_id ~= "" and item.call_id or nil
				local match_idx
				if cid then
					for idx, entry in ipairs(P.pending_tool_search) do
						if entry.call_id == cid then
							match_idx = idx
							break
						end
					end
				elseif #P.pending_tool_search > 0 then
					match_idx = 1
				end
				if match_idx then
					local entry = table.remove(P.pending_tool_search, match_idx)
					local call = entry.block
					call.native = call.native or {}
					call.native[WIRE] = call.native[WIRE] or {}
					call.native[WIRE].output_item = item
					local status = type(item.status) == "string" and item.status or "completed"
					call.result = {
						status = status,
						tools = mare.json.array(extract_tool_search_names(item.tools)),
					}
					sink({
						type = "hosted_tool_done",
						name = "tool_search",
						id = call.id,
						status = status,
						result = call.result,
					})
				else
					table.insert(P.content, {
						type = WIRE .. ".tool_search_output",
						native = { [WIRE] = { item = item } },
					})
				end
			elseif item.type == "function_call" then
				local call_id, name = resolve_tool_call_identity(item, d.output_index)
				local args, perr = require("mare_lib").parse_args(item.arguments)
				if not perr and healer_mod and type(args) == "table" then
					args = healer_mod.clean_arguments(heal_pattern, args)
				end
				table.insert(P.content, {
					type = "tool_call",
					id = call_id,
					name = name,
					arguments = args,
					parse_error = perr,
					raw_arguments = perr and item.arguments or nil,
				})
				P.stop = "tool_use"
			elseif item.type == "custom_tool_call" then
				local call_id, name = resolve_tool_call_identity(item, d.output_index)
				-- freeform input arrives raw; tools see it as arguments.input
				table.insert(P.content, {
					type = "tool_call",
					id = call_id,
					name = name,
					arguments = type(item.input) == "string" and { input = item.input } or nil,
					parse_error = type(item.input) ~= "string" and "custom tool input must be a string" or nil,
				})
				P.stop = "tool_use"
			end
		elseif t == "codex.response.metadata" or t == "response.metadata" then
			if type(opts.on_metadata) == "function" then
				opts.on_metadata(type(d.headers) == "table" and d.headers or {}, d)
			end
		elseif t == "response.completed" or t == "response.incomplete" or t == "response.done" then
			local r = d.response or {}
			local status = type(r.status) == "string" and r.status or nil
			if t == "response.done" and status == "failed" then
				local code = r.error and r.error.code
				P.error = (r.error and r.error.message)
					or (code and ("response failed: " .. tostring(code)))
					or "response failed"
				P.error_code = code
				P.error_obj = r.error
				return true
			end
			if t == "response.done" and status == "cancelled" then
				local code = (r.error and r.error.code) or "cancelled"
				P.error = (r.error and r.error.message) or "response cancelled"
				P.error_code = code
				P.error_obj = r.error or { code = code }
				return true
			end
			P.completed = true
			if type(r.usage) == "table" and next(r.usage) ~= nil then
				P.usage = M.usage(r.usage)
			end
			if t == "response.incomplete" or (t == "response.done" and status == "incomplete") then
				-- max_output_tokens → a normal "length" stop (the agent fails the truncated tool
				-- calls); any other reason (content filter, …) is an error naming it
				local reason = r.incomplete_details and r.incomplete_details.reason or "unknown"
				P.incomplete_reason = reason
				if reason == "max_output_tokens" then
					P.stop = "length"
				else
					P.error = "Incomplete response returned, reason: " .. tostring(reason)
					P.error_code = "incomplete"
					P.error_obj = { code = "incomplete", reason = reason }
				end
			end
			return true
		elseif t == "response.failed" then
			local r = d.response or {}
			local code = r.error and r.error.code
			P.error = (r.error and r.error.message)
				or (code and ("response failed: " .. tostring(code)))
				or "response failed"
			P.error_code = code
			P.error_obj = r.error
			return true
		elseif t == "error" then
			P.error = d.message or (d.error and d.error.message) or ev.data
			P.error_code = d.code or (d.error and d.error.code)
			P.error_obj = d.error or d
			return true
		end
		return false
	end

	function P.finish()
		if P.error then
			local raw = {}
			if type(P.error_obj) == "table" then
				for k, v in pairs(P.error_obj) do
					raw[k] = v
				end
			end
			raw.message = P.error
			raw.code = raw.code ~= nil and raw.code or P.error_code
			if on_error then
				error(on_error(raw, P.error) or raw)
			end
			error(raw)
		end
		if not P.completed then
			-- A stream that ends without response.completed was cut off; retry it.
			error({ message = "stream ended before response.completed", kind = "stream_closed" })
		end
		for _, entry in ipairs(P.pending_tool_search) do
			local call = entry.block
			local status = call.result and call.result.status or "completed"
			sink({
				type = "hosted_tool_done",
				name = "tool_search",
				id = call.id,
				status = status,
				result = call.result,
			})
		end
		P.pending_tool_search = {}
		local has_tool = false
		for _, b in ipairs(P.content) do
			if b.type == "tool_call" and not b.hosted then
				has_tool = true
			end
		end
		return {
			role = "assistant",
			content = P.content,
			usage = P.usage or {},
			-- an incomplete response stays "length" even with tool calls: they never run
			stop_reason = (P.stop ~= "length" and has_tool) and "tool_use" or P.stop,
		}
	end

	return P
end

return M
