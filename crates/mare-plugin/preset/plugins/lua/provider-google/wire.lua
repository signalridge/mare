-- Google Generative Language wire helpers: message conversion, tool conversion, error metadata,
-- and SSE/JSON response parser; pure module returning its table `M` directly.
local M = {}
local WIRE = "google-generate"

local function native_of(obj)
	return type(obj) == "table"
			and type(obj.native) == "table"
			and type(obj.native[WIRE]) == "table"
			and obj.native[WIRE]
		or nil
end

local function thought_sig_of(b)
	local ng = native_of(b)
	local sig = ng and ng.thought_signature
	return (type(sig) == "string" and sig ~= "") and sig or nil
end
-- Gemini's wording for too many / too large images in one request
local function IMAGE_LIMIT(msg)
	local m = string.lower(tostring(msg or ""))
	return m:find("too many images", 1, true) ~= nil
		or m:find("number of images", 1, true) ~= nil
		or m:find("image size exceeds", 1, true) ~= nil
		or m:find("image is too large", 1, true) ~= nil
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

M.IMAGE_LIMIT = IMAGE_LIMIT
M.OVERFLOW = OVERFLOW
M.ERRORS = {
	codes = { RESOURCE_EXHAUSTED = "rate_limit" },
	overflow = OVERFLOW,
	image_limit = IMAGE_LIMIT,
}

local function gemini_major_version(model_id)
	if type(model_id) ~= "string" then
		return nil
	end
	local lower = string.lower(model_id)
	local major = lower:match("^gemini%-live%-(%d+)") or lower:match("^gemini%-(%d+)")
	return major and tonumber(major) or nil
end

-- Gemini 3 multimodal functionResponse.parts only accepts PNG, JPEG, and WebP
-- other image mimes go in the trailing user turn.
local FUNCTION_RESPONSE_IMAGE_MIMES = {
	["image/png"] = true,
	["image/jpeg"] = true,
	["image/webp"] = true,
}

local GEMINI_IMAGE_MIMES = {
	["image/png"] = true,
	["image/jpeg"] = true,
	["image/webp"] = true,
	["image/heic"] = true,
	["image/heif"] = true,
}

local function sanitize_gemini_image(b)
	local raw_mime = type(b.mime) == "string" and b.mime or "image/png"
	local mt = raw_mime:lower():match("^%s*(.-)%s*$")
	mt = (mt:match("^([^;]+)") or mt):match("^%s*(.-)%s*$")
	if mt == "image/jpg" then
		mt = "image/jpeg"
	end
	if GEMINI_IMAGE_MIMES[mt] then
		return { inlineData = { mimeType = mt, data = b.data } }
	end
	if type(b.data) == "string" then
		local png = mare.image.to_png(b.data)
		if png then
			return { inlineData = { mimeType = png.mime or "image/png", data = png.data } }
		end
	end
	local label = mt ~= "" and mt or "unknown"
	return { text = "[An image of type " .. label .. " was left out: Gemini reads PNG, JPEG, WebP, HEIC and HEIF.]" }
end

local function supports_multimodal_function_response(model_id)
	local major = gemini_major_version(model_id)
	if major ~= nil then
		return major >= 3
	end
	return true
end

local function requires_tool_call_id(model_id)
	if type(model_id) ~= "string" then
		return false
	end
	local lower = string.lower(model_id)
	local ver = gemini_major_version(model_id)
	return lower:sub(1, 7) == "claude-" or lower:sub(1, 8) == "gpt-oss-" or (ver ~= nil and ver >= 3)
end

local function requires_thought_signature(model_id)
	local ver = gemini_major_version(model_id)
	return ver ~= nil and ver >= 3
end

local function is_valid_thought_signature(sig)
	if type(sig) ~= "string" or sig == "" or (#sig % 4 ~= 0) then
		return false
	end
	local body, pad = sig:match("^([A-Za-z0-9+/]+)(=*)$")
	return body ~= nil and #pad <= 2
end

function M.convert_messages(messages, model_id, own_provider)
	own_provider = own_provider or "google"
	local include_id = requires_tool_call_id(model_id)
	local need_sig = requires_thought_signature(model_id)
	local contents = {}
	local names = {} -- tool_call_id -> function name
	local pending_tool_images = {}
	local function push(role, parts)
		if #parts == 0 then
			return
		end
		local last = contents[#contents]
		if last and last.role == role then
			for _, p in ipairs(parts) do
				table.insert(last.parts, p)
			end
		else
			contents[#contents + 1] = { role = role, parts = parts }
		end
	end
	local function flush_tool_images()
		if #pending_tool_images == 0 then
			return
		end
		local parts = { { text = "Tool result image:" } }
		for _, img in ipairs(pending_tool_images) do
			parts[#parts + 1] = img
		end
		-- Keep tool result images in a separate user turn for Gemini < 3 instead of merging into
		-- the preceding functionResponse turn.
		contents[#contents + 1] = { role = "user", parts = parts }
		pending_tool_images = {}
	end
	for _, m in ipairs(messages) do
		if m.role == "user" then
			flush_tool_images()
			local parts = {}
			for _, b in ipairs(m.content) do
				if b.type == "text" and b.text ~= "" then
					parts[#parts + 1] = { text = b.text }
				elseif b.type == "mention" then
					local label = b.name and ("@" .. b.path .. " (" .. b.name .. ")") or ("@" .. b.path)
					parts[#parts + 1] = { text = label }
				elseif b.type == "image" then
					parts[#parts + 1] = sanitize_gemini_image(b)
				end
			end
			push("user", parts)
		elseif m.role == "assistant" then
			flush_tool_images()
			local same = (m.provider_id == nil or m.provider_id == own_provider)
				and (model_id == nil or m.model_id == nil or m.model_id == model_id)
			-- Google requires the first function call of each step to carry a signature; a
			-- signature on another part does not stand in for it. Later parallel calls are
			-- replayed as received (normally unsigned).
			local first_call = true
			local parts = {}
			for _, b in ipairs(m.content) do
				local raw_sig = same and thought_sig_of(b) or nil
				local sig = (raw_sig and is_valid_thought_signature(raw_sig)) and raw_sig or nil
				if b.type == "reasoning" then
					if sig ~= nil then
						parts[#parts + 1] = { text = b.text or "", thought = true, thoughtSignature = sig }
					end
				elseif b.type == "text" then
					local has_text = type(b.text) == "string" and b.text:match("%S") ~= nil
					if has_text or sig ~= nil then
						parts[#parts + 1] = { text = b.text or "", thoughtSignature = sig }
					end
				elseif b.type == "tool_call" and not b.hosted then
					names[b.id] = b.name
					local args = b.arguments
					if type(args) ~= "table" then
						args = {}
					end
					local fc = { name = b.name, args = args }
					if include_id then
						fc.id = b.id
					end
					local call_sig = sig
					if call_sig == nil and need_sig and first_call then
						call_sig = "skip_thought_signature_validator"
					end
					first_call = false
					parts[#parts + 1] = {
						functionCall = fc,
						thoughtSignature = call_sig,
					}
				end
			end
			push("model", parts)
		elseif m.role == "tool_result" then
			local texts, nested_images, saw_image = {}, {}, false
			local can_nest = supports_multimodal_function_response(model_id)
			for _, b in ipairs(m.content) do
				if b.type == "text" then
					texts[#texts + 1] = b.text
				elseif b.type == "image" then
					saw_image = true
					local part = sanitize_gemini_image(b)
					if part.inlineData then
						local norm_mime = part.inlineData.mimeType
						if can_nest and FUNCTION_RESPONSE_IMAGE_MIMES[norm_mime] then
							nested_images[#nested_images + 1] = part
						else
							pending_tool_images[#pending_tool_images + 1] = part
						end
					elseif part.text then
						texts[#texts + 1] = part.text
					end
				end
			end
			local text_result = table.concat(texts, "\n")
			local has_text = text_result:match("%S") ~= nil
			-- an empty result still needs content the API accepts; images travel as parts
			local response_value = has_text and text_result
				or (saw_image and "(see attached image)" or "(no tool output)")
			local response = m.is_error and { error = response_value } or { output = response_value }
			local fn_resp = {
				name = m.tool_name or names[m.tool_call_id] or "tool",
				response = response,
			}
			if include_id then
				fn_resp.id = m.tool_call_id
			end
			if #nested_images > 0 then
				fn_resp.parts = mare.json.array(nested_images)
			end
			push("user", { { functionResponse = fn_resp } })
		end
	end
	flush_tool_images()
	return contents
end

function M.convert_tools(tools, dialect)
	if not tools or #tools == 0 then
		return nil
	end
	local sanitize = dialect and require("mare_lib.schema_sanitize").sanitize or nil
	local decls = {}
	for _, t in ipairs(tools) do
		if t.type ~= "hosted" then
			local params = sanitize and sanitize(t.parameters, dialect) or t.parameters
			decls[#decls + 1] = { name = t.name, description = t.description, parametersJsonSchema = params }
		end
	end
	if #decls == 0 then
		return nil
	end
	return { { functionDeclarations = decls } }
end

-- fallback tool-call ids: name, time and a process-wide counter, so they are stable for the
-- call and unique across chunks and turns
local call_counter = 0

--- Parse and strictly validate a JSONPath string like "$.operations[0].action" or "$.items[]".
local function parse_json_path(raw_path)
	if type(raw_path) ~= "string" or raw_path == "" or raw_path:sub(1, 1) ~= "$" then
		return nil, false
	end
	local is_array_term = raw_path:sub(-2) == "[]"
	local path = is_array_term and raw_path:sub(1, -3) or raw_path
	if #path <= 1 then
		return nil, is_array_term
	end
	local segments = {}
	local pos = 2
	local len = #path
	while pos <= len do
		local ch = path:sub(pos, pos)
		if ch == "." then
			pos = pos + 1
			if pos > len then
				return nil, is_array_term
			end
			local next_ch = path:sub(pos, pos)
			if next_ch == "." or next_ch == "[" then
				return nil, is_array_term
			end
			local dot_or_br = path:find("[%.%[]", pos)
			local key = dot_or_br and path:sub(pos, dot_or_br - 1) or path:sub(pos)
			if key == "" or key:find("]", 1, true) then
				return nil, is_array_term
			end
			segments[#segments + 1] = key
			pos = dot_or_br or (len + 1)
		elseif ch == "[" then
			local close = path:find("]", pos + 1, true)
			if not close then
				return nil, is_array_term
			end
			local inner = path:sub(pos + 1, close - 1)
			if not inner:match("^%d+$") then
				return nil, is_array_term
			end
			segments[#segments + 1] = tonumber(inner)
			pos = close + 1
			if pos <= len then
				local after = path:sub(pos, pos)
				if after ~= "." and after ~= "[" then
					return nil, is_array_term
				end
			end
		else
			return nil, is_array_term
		end
	end
	return #segments > 0 and segments or nil, is_array_term
end

--- Apply Gemini 3 streaming `partialArgs` into `target` table. Returns an error string if invalid.
local function apply_partial_args(target, partial_args, lossless_partial_args, incomplete_paths)
	local json_null = mare.json and mare.json.null
	for i, arg in ipairs(partial_args) do
		if type(arg) ~= "table" then
			return "partialArgs entry is not an object"
		end
		local larg = type(lossless_partial_args) == "table" and lossless_partial_args[i] or nil
		local segments, is_array_term = parse_json_path(arg.jsonPath)
		if not segments then
			return "partialArgs entry missing or invalid jsonPath"
		end
		if incomplete_paths then
			if arg.willContinue == true then
				incomplete_paths[arg.jsonPath] = true
			else
				incomplete_paths[arg.jsonPath] = nil
			end
		end
		local cur = target
		for s = 1, #segments - 1 do
			local seg = segments[s]
			local key = type(seg) == "number" and (seg + 1) or seg
			local next_seg = segments[s + 1]
			if cur[key] == nil or cur[key] == json_null then
				cur[key] = type(next_seg) == "number" and mare.json.array() or {}
			end
			cur = cur[key]
		end
		local leaf = segments[#segments]
		local leaf_key = type(leaf) == "number" and (leaf + 1) or leaf
		local existing = cur[leaf_key]
		if arg.stringValue ~= nil then
			if type(existing) == "string" then
				cur[leaf_key] = existing .. arg.stringValue
			else
				cur[leaf_key] = arg.stringValue
			end
		elseif arg.numberValue ~= nil then
			cur[leaf_key] = larg and larg.numberValue or arg.numberValue
		elseif arg.boolValue ~= nil then
			cur[leaf_key] = arg.boolValue
		elseif arg.nullValue ~= nil or (larg and larg.nullValue ~= nil) then
			cur[leaf_key] = json_null
		elseif existing == nil and arg.willContinue ~= true then
			cur[leaf_key] = is_array_term and mare.json.array() or {}
		end
	end
	return nil
end

--- new_parser(sink, on_error?) — `on_error(api_err)` turns a mid-stream `{"error": …}` into
--- the error raised at the end.
function M.new_parser(sink, on_error)
	local stream = require("mare_lib")
	local P = { content = {}, usage = nil, finish = nil, ids = {}, active_tool = nil }
	local current_record_id = nil
	local parser_call_seq = 0

	local function finalize_active_tool(from_end)
		local tc = P.active_tool
		if not tc then
			return
		end
		P.active_tool = nil
		local perr = tc.parse_error
		if not perr and (next(tc.incomplete_paths) ~= nil or (from_end and tc.incomplete_call)) then
			perr = "incomplete streaming functionCall (willContinue=true)"
		end
		P.content[#P.content + 1] = {
			type = "tool_call",
			id = tc.id,
			name = tc.name,
			arguments = not perr and tc.args or nil,
			parse_error = perr,
			native = (type(tc.signature) == "string" and tc.signature ~= "") and {
				[WIRE] = { thought_signature = tc.signature },
			} or nil,
		}
	end

	local function next_call_id(fc)
		local id = type(fc.id) == "string" and fc.id ~= "" and fc.id or nil
		if not id or P.ids[id] then
			if type(current_record_id) == "string" and current_record_id ~= "" then
				parser_call_seq = parser_call_seq + 1
				local seed = ("%s:%s"):format(current_record_id, fc.name or "call")
				id, parser_call_seq = stream.next_hashed_call_id(P.ids, seed, 24, parser_call_seq)
			else
				call_counter = call_counter + 1
				id = ("%s_%d_%d"):format(fc.name or "call", math.floor(mare.task.now()), call_counter)
			end
		end
		P.ids[id] = true
		return id
	end

	-- Part boundaries. A delta continues the last block of its kind (backends may sign only
	-- the first delta of a part, or only the last retainThoughtSignature),
	-- except that two signed deltas are two parts whatever their opaque values, and a
	-- signature-only empty part is complete on its own. Identity never comes from signature
	-- values.
	local open_has_text = false
	local function block_for(kind, sig, text)
		local b = P.content[#P.content]
		local prev_sig = b and thought_sig_of(b) or nil
		local continues = b ~= nil
			and b.type == kind
			and not (sig and prev_sig)
			and not (prev_sig and not open_has_text)
		if not continues then
			b = stream.stream_record({ type = kind, text = "" }, { "text" })
			P.content[#P.content + 1] = b
			open_has_text = false
		end
		if sig then
			b.native = b.native or {}
			b.native[WIRE] = b.native[WIRE] or {}
			b.native[WIRE].thought_signature = sig
		end
		if text ~= "" then
			open_has_text = true
		end
		return b
	end

	function P.feed(ev)
		local data = ev.data or ""
		if not data:match("%S") then
			return
		end
		local d = mare.json.decode(data)
		if type(d) ~= "table" then
			error({ message = "Google: malformed stream event", kind = "malformed_response" })
		end
		if type(d.responseId) == "string" and d.responseId ~= "" then
			current_record_id = d.responseId
		end
		if d.error then
			P.error = type(d.error) == "table" and d.error or { message = tostring(d.error) }
			return
		end
		-- the prompt itself was blocked: no candidates, no finishReason
		local pf = d.promptFeedback
		if type(pf) == "table" and type(pf.blockReason) == "string" and pf.blockReason ~= "" then
			P.blocked = pf.blockReason
		end
		local c = d.candidates and d.candidates[1]
		if c then
			local lossless_parts
			for part_index, part in ipairs(c.content and c.content.parts or {}) do
				if part.functionCall then
					-- Text-only chunks do not need a second parse. Decode only chunks that
					-- actually carry tool arguments, retaining null and empty containers.
					if not lossless_parts then
						local lossless = mare.json.decode_lossless(data)
						local candidate = lossless.candidates[1]
						lossless_parts = type(candidate.content) == "table" and candidate.content.parts or {}
					end
					local fc = part.functionCall
					local lfc = lossless_parts[part_index].functionCall
					local is_streaming = fc.partialArgs ~= nil or (fc.name ~= nil and fc.willContinue == true)
					local is_terminal = fc.name == nil
						and fc.args == nil
						and fc.partialArgs == nil
						and fc.willContinue == nil
					if is_streaming then
						if fc.name ~= nil then
							finalize_active_tool(true)
							local id = next_call_id(fc)
							sink({ type = "tool_call_start", tool_call_id = id, name = fc.name })
							P.active_tool = {
								id = id,
								name = fc.name,
								args = {},
								signature = part.thoughtSignature,
								incomplete_call = fc.willContinue == true,
								incomplete_paths = {},
							}
						end
						if P.active_tool and type(fc.partialArgs) == "table" then
							if part.thoughtSignature and not P.active_tool.signature then
								P.active_tool.signature = part.thoughtSignature
							end
							local perr = apply_partial_args(
								P.active_tool.args,
								fc.partialArgs,
								lfc and lfc.partialArgs,
								P.active_tool.incomplete_paths
							)
							if perr and not P.active_tool.parse_error then
								P.active_tool.parse_error = perr
							end
							P.active_tool.incomplete_call = fc.willContinue == true
							if not P.active_tool.incomplete_call and next(P.active_tool.incomplete_paths) == nil then
								finalize_active_tool(false)
							end
						end
					elseif is_terminal and P.active_tool then
						P.active_tool.incomplete_call = false
						finalize_active_tool(false)
					else
						if type(fc.name) ~= "string" or fc.name == "" then
							error({ message = "Google: functionCall missing name", kind = "malformed_response" }, 0)
						end
						finalize_active_tool(true)
						local id = next_call_id(fc)
						sink({ type = "tool_call_start", tool_call_id = id, name = fc.name })
						local arguments = lfc.args
						if arguments == nil then
							arguments = {}
						end
						P.content[#P.content + 1] = {
							type = "tool_call",
							id = id,
							name = fc.name,
							arguments = arguments,
							parse_error = type(arguments) ~= "table" and "arguments are not a JSON object" or nil,
							native = (type(part.thoughtSignature) == "string" and part.thoughtSignature ~= "") and {
								[WIRE] = { thought_signature = part.thoughtSignature },
							} or nil,
						}
					end
				elseif part.text ~= nil then
					local kind = part.thought and "reasoning" or "text"
					local sig = part.thoughtSignature
					if type(sig) ~= "string" or sig == "" then
						sig = nil
					end
					local b = block_for(kind, sig, part.text)
					stream.stream_append(b, "text", part.text)
					if part.text ~= "" then
						sink({ type = kind == "reasoning" and "reasoning_delta" or "text_delta", delta = part.text })
					end
				end
			end
			if type(c.finishReason) == "string" and c.finishReason ~= "" then
				P.finish = c.finishReason -- Google's end signal
			end
		end
		local u = d.usageMetadata
		if u then
			local cached = u.cachedContentTokenCount or 0
			local input_total = (u.promptTokenCount or 0) + (u.toolUsePromptTokenCount or 0)
			P.usage = {
				input = math.max(0, input_total - cached),
				output = (u.candidatesTokenCount or 0) + (u.thoughtsTokenCount or 0),
				cache_read = cached,
				cache_write = 0,
				reasoning = tonumber(u.thoughtsTokenCount),
			}
		end
	end

	function P.finish_message()
		if P.error then
			if on_error then
				error(on_error(P.error), 0)
			end
			error({ message = "Google: " .. (P.error.message or mare.json.encode(P.error)) })
		end
		if P.blocked and not P.finish then
			-- retrying the same prompt is blocked again: a refusal, not a cut-off stream
			error({
				message = ("Google: the prompt was blocked (%s); rephrase the request"):format(P.blocked),
				kind = "refusal",
			})
		end
		if not P.finish then
			error({ message = "Google: stream closed before finishReason", kind = "stream_closed" })
		end
		finalize_active_tool(true)
		local has_tool = false
		local content = {}
		for _, b in ipairs(P.content) do
			content[#content + 1] = stream.stream_materialize(b)
			if b.type == "tool_call" and not b.hosted then
				has_tool = true
			end
		end
		-- truncation wins over tool calls: an incomplete response never runs tools
		local FILTERED = {
			SAFETY = true,
			RECITATION = true,
			PROHIBITED_CONTENT = true,
			BLOCKLIST = true,
			SPII = true,
			IMAGE_SAFETY = true,
			IMAGE_PROHIBITED_CONTENT = true,
			IMAGE_RECITATION = true,
			IMAGE_OTHER = true,
		}
		local msg = { role = "assistant", content = content, usage = P.usage or {} }
		if P.finish == "MAX_TOKENS" or P.finish == "CONTINUATION" then
			msg.stop_reason = "length"
		elseif FILTERED[P.finish] then
			msg.stop_reason = "filtered"
			msg.raw_stop_reason = P.finish
			msg.error = ("the model output was filtered (%s)"):format(P.finish)
		elseif P.finish == "STOP" then
			msg.stop_reason = has_tool and "tool_use" or "stop"
		else
			-- MALFORMED_FUNCTION_CALL, OTHER, etc. are errors, not "stop"
			msg.stop_reason = "error"
			msg.raw_stop_reason = P.finish
			msg.error = "Provider finishReason: " .. tostring(P.finish)
		end
		return msg
	end

	return P
end

return M
