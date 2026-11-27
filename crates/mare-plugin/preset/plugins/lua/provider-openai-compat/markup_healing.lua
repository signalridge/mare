-- provider-openai-compat.markup_healing: provider-side streaming healing and replay stripping
-- for leaked tool-call markup (DeepSeek DSML & V3 template tokens, Kimi section tokens, Qwen
-- XML & Qwen3 JSON). Loaded lazily only when a request targets an eligible model/dialect.

local M = {}

local FULLWIDTH_BAR = "\239\189\156" -- U+FF5C '｜'
local FULLWIDTH_UNDERSCORE = "\226\150\129" -- U+2581 '▁'

local DSML_OPEN_FW = "<" .. FULLWIDTH_BAR .. "DSML" .. FULLWIDTH_BAR .. "tool_calls>"
local DSML_CLOSE_FW = "</" .. FULLWIDTH_BAR .. "DSML" .. FULLWIDTH_BAR .. "tool_calls>"
local DSML_OPEN_ASCII = "<|DSML|tool_calls>"
local DSML_CLOSE_ASCII = "</|DSML|tool_calls>"
local DSML_INVOKE_OPEN_FW = "<" .. FULLWIDTH_BAR .. "DSML" .. FULLWIDTH_BAR .. "invoke"
local DSML_INVOKE_OPEN_ASCII = "<|DSML|invoke"
local DSML_INVOKE_CLOSE_FW = "</" .. FULLWIDTH_BAR .. "DSML" .. FULLWIDTH_BAR .. "invoke>"
local DSML_INVOKE_CLOSE_ASCII = "</|DSML|invoke>"
local DSML_PARAM_OPEN_FW = "<" .. FULLWIDTH_BAR .. "DSML" .. FULLWIDTH_BAR .. "parameter"
local DSML_PARAM_OPEN_ASCII = "<|DSML|parameter"
local DSML_PARAM_CLOSE_FW = "</" .. FULLWIDTH_BAR .. "DSML" .. FULLWIDTH_BAR .. "parameter>"
local DSML_PARAM_CLOSE_ASCII = "</|DSML|parameter>"

local DS_CALLS_BEGIN = "<"
	.. FULLWIDTH_BAR
	.. "tool"
	.. FULLWIDTH_UNDERSCORE
	.. "calls"
	.. FULLWIDTH_UNDERSCORE
	.. "begin"
	.. FULLWIDTH_BAR
	.. ">"
local DS_CALLS_END = "<"
	.. FULLWIDTH_BAR
	.. "tool"
	.. FULLWIDTH_UNDERSCORE
	.. "calls"
	.. FULLWIDTH_UNDERSCORE
	.. "end"
	.. FULLWIDTH_BAR
	.. ">"
local DS_CALL_BEGIN = "<"
	.. FULLWIDTH_BAR
	.. "tool"
	.. FULLWIDTH_UNDERSCORE
	.. "call"
	.. FULLWIDTH_UNDERSCORE
	.. "begin"
	.. FULLWIDTH_BAR
	.. ">"
local DS_CALL_END = "<"
	.. FULLWIDTH_BAR
	.. "tool"
	.. FULLWIDTH_UNDERSCORE
	.. "call"
	.. FULLWIDTH_UNDERSCORE
	.. "end"
	.. FULLWIDTH_BAR
	.. ">"
local DS_SEP = "<" .. FULLWIDTH_BAR .. "tool" .. FULLWIDTH_UNDERSCORE .. "sep" .. FULLWIDTH_BAR .. ">"

local KIMI_SECTION_BEGIN = "<|tool_calls_section_begin|>"
local KIMI_SECTION_END = "<|tool_calls_section_end|>"
local KIMI_CALL_BEGIN = "<|tool_call_begin|>"
local KIMI_CALL_END = "<|tool_call_end|>"
local KIMI_ARG_BEGIN = "<|tool_call_argument_begin|>"

local QWEN_XML_OPEN = "<tool_calls>"
local QWEN_XML_CLOSE = "</tool_calls>"
local QWEN3_OPEN = "<tool_call>"
local QWEN3_CLOSE = "</tool_call>"

local VALID_PATTERNS = { dsml = true, kimi = true, qwen = true }

local function host_of(url)
	if type(url) ~= "string" then
		return ""
	end
	return (url:match("^https?://([^/:?#]+)") or ""):lower()
end

local function host_matches(host, domain)
	if host == "" then
		return false
	end
	return host == domain or (#host > #domain and host:sub(-(#domain + 1)) == "." .. domain)
end

local function id_matches(s, prefix)
	if type(s) ~= "string" or s == "" then
		return false
	end
	if s == prefix then
		return true
	end
	if s:match("^" .. prefix .. "[-_/.]") ~= nil then
		return true
	end
	return s:find("/" .. prefix, 1, true) ~= nil
end

--- Resolve whether leaked tool-call markup healing applies for this provider endpoint and model.
--- Returns "dsml", "kimi", "qwen", or nil.
function M.resolve_pattern(ep, model, base_url)
	local ep_cfg = type(ep) == "table" and type(ep.endpoint_config) == "table" and ep.endpoint_config or nil
	local prov_cfg = type(ep) == "table" and type(ep.provider_config) == "table" and ep.provider_config or nil
	for _, explicit in ipairs({
		type(model) == "table" and model.markup_healing or nil,
		ep_cfg and ep_cfg.markup_healing or nil,
		prov_cfg and prov_cfg.markup_healing or nil,
		type(ep) == "table" and ep.markup_healing or nil,
	}) do
		if explicit == false or explicit == "none" or explicit == "off" then
			return nil
		end
		if type(explicit) == "string" and VALID_PATTERNS[explicit] then
			return explicit
		end
	end
	local d = type(ep) == "table" and type(ep.dialect) == "string" and ep.dialect:lower() or ""
	local host = host_of(base_url or (type(ep) == "table" and (ep.base_url or ep.url) or nil))
	local mid = ""
	if type(model) == "table" and type(model.id) == "string" then
		mid = model.id:lower()
	elseif type(model) == "string" then
		mid = model:lower()
	end
	if id_matches(d, "deepseek") or host_matches(host, "deepseek.com") or mid:find("deepseek", 1, true) ~= nil then
		return "dsml"
	end
	if
		id_matches(d, "kimi")
		or id_matches(d, "moonshot")
		or host_matches(host, "kimi.com")
		or host_matches(host, "moonshot.ai")
		or host_matches(host, "moonshot.cn")
		or id_matches(mid, "kimi")
		or id_matches(mid, "moonshot")
	then
		return "kimi"
	end
	if
		id_matches(d, "qwen")
		or host_matches(host, "dashscope.aliyuncs.com")
		or host_matches(host, "dashscope-intl.aliyuncs.com")
		or mid:find("qwen", 1, true) ~= nil
	then
		return "qwen"
	end
	return nil
end

-------------------------------------------------------------------------------
-- Linear-time terminator wait helper (avoids O(n^2) scanning on large args)
-------------------------------------------------------------------------------

local function new_terminator_wait()
	local W = { term = "", tail = "", parts = {} }
	function W:arm(term, buffered)
		self.term = term
		local keep = #term - 1
		if keep <= 0 then
			self.tail = ""
		elseif #buffered > keep then
			self.tail = buffered:sub(#buffered - keep + 1)
		else
			self.tail = buffered
		end
		self.parts = {}
	end
	function W:absorb(chunk)
		if self.term == "" then
			return false
		end
		local win = self.tail .. chunk
		if win:find(self.term, 1, true) then
			return false
		end
		self.parts[#self.parts + 1] = chunk
		local keep = #self.term - 1
		if keep <= 0 then
			self.tail = ""
		elseif #win > keep then
			self.tail = win:sub(#win - keep + 1)
		else
			self.tail = win
		end
		return true
	end
	function W:release(buffered)
		self.term = ""
		self.tail = ""
		if #self.parts == 0 then
			return buffered
		end
		local joined = buffered .. table.concat(self.parts)
		self.parts = {}
		return joined
	end
	return W
end

-------------------------------------------------------------------------------
-- Shared string / JSON / Markdown code-span helpers
-------------------------------------------------------------------------------

local function starts_with(s, prefix)
	return #s >= #prefix and s:sub(1, #prefix) == prefix
end

local function trim(s)
	return (s:gsub("^%s+", ""):gsub("%s+$", ""))
end

local function trim_start(s)
	return (s:gsub("^%s+", ""))
end

local function trim_end(s)
	return (s:gsub("%s+$", ""))
end

local function partial_suffix_overlap(text, target)
	local max_len = math.min(#text, #target - 1)
	for len = max_len, 1, -1 do
		if text:sub(#text - len + 1) == target:sub(1, len) then
			return len
		end
	end
	return 0
end

local function partial_suffix_overlap_any(text, targets)
	local best = 0
	for _, t in ipairs(targets) do
		local k = partial_suffix_overlap(text, t)
		if k > best then
			best = k
		end
	end
	return best
end

local function find_earliest_literal(text, tokens, from)
	from = from or 1
	local best_i, best_tok
	for _, tok in ipairs(tokens) do
		local i = text:find(tok, from, true)
		if i and (not best_i or i < best_i or (i == best_i and #tok > #best_tok)) then
			best_i, best_tok = i, tok
		end
	end
	return best_i, best_tok
end

local function parse_json_object(raw)
	local t = trim(raw)
	if t == "" then
		return {}
	end
	local ok, decoded = pcall(mare.json.decode, t)
	if ok and type(decoded) == "table" then
		return decoded
	end
	return {}
end

local function coerce_dsml_value(raw, is_string)
	if is_string then
		return raw
	end
	local t = trim(raw)
	if t == "" then
		return raw
	end
	local ok, decoded = pcall(mare.json.decode, t)
	if ok then
		return decoded
	end
	return raw
end

local function normalize_kimi_name(raw_id)
	local before_colon = raw_id:match("^([^:]*)") or raw_id
	local last_part = before_colon:match("([^.]+)$") or before_colon
	return trim(last_part)
end

-- Markdown code fence/span state: fence = opening run length (0 outside code),
-- fence_char = 96 ('`') or 126 ('~'), run = pending run length, run_char = pending char,
-- newline = last non-space char was '\n'.
local function settle_fence_run(fence, fence_char, run, run_char)
	if run == 0 then
		return fence, fence_char
	end
	if fence == 0 then
		if run_char == 96 then
			return run, 96
		end
		if run_char == 126 and run >= 3 then
			return run, 126
		end
		return 0, 0
	end
	if run_char == fence_char then
		if (fence >= 3 and run >= fence) or (fence < 3 and run == fence) then
			return 0, 0
		end
	end
	return fence, fence_char
end

local function settle_backtick_run(fence, run, fence_char, run_char)
	local f = settle_fence_run(fence, fence_char or (fence > 0 and 96 or 0), run, run_char or (run > 0 and 96 or 0))
	return f
end

local function advance_code_state(state, text)
	local fence = state.fence
	local fence_char = state.fence_char or (fence > 0 and 96 or 0)
	local run = state.run
	local run_char = state.run_char or (run > 0 and 96 or 0)
	local newline = state.newline
	for i = 1, #text do
		local b = text:byte(i)
		if b == 96 or b == 126 then -- '`' or '~'
			if run > 0 and run_char ~= b then
				fence, fence_char = settle_fence_run(fence, fence_char, run, run_char)
				run = 0
			end
			run = run + 1
			run_char = b
			newline = false
		else
			fence, fence_char = settle_fence_run(fence, fence_char, run, run_char)
			run, run_char = 0, 0
			if b == 10 then -- '\n'
				if newline and fence < 3 then
					fence, fence_char = 0, 0
				end
				newline = true
			elseif b ~= 32 and b ~= 9 and b ~= 13 then
				newline = false
			end
		end
	end
	return { fence = fence, fence_char = fence_char, run = run, run_char = run_char, newline = newline }
end

local function inline_span_closes(probe, text, from, final)
	local fence, run, newline = probe.fence, probe.run, probe.newline
	for i = from, #text do
		local b = text:byte(i)
		if b == 96 then
			run = run + 1
			newline = false
		else
			if run == fence then
				return true
			end
			run = 0
			if b == 10 then
				if newline then
					return false
				end
				newline = true
			elseif b ~= 32 and b ~= 9 and b ~= 13 then
				newline = false
			end
		end
	end
	probe.run, probe.newline = run, newline
	if final then
		return run == fence
	end
	return nil
end

-------------------------------------------------------------------------------
-- DeepSeek V3 template token recognition (canonical & re-spelled with >= 1 ｜)
-------------------------------------------------------------------------------

local DS_TEMPLATE_BY_KEY = {
	tool_calls_begin = DS_CALLS_BEGIN,
	tool_calls_end = DS_CALLS_END,
	tool_call_begin = DS_CALL_BEGIN,
	tool_call_end = DS_CALL_END,
	tool_sep = DS_SEP,
}

local DS_TEMPLATE_KEYS = {
	"tool_calls_begin",
	"tool_calls_end",
	"tool_call_begin",
	"tool_call_end",
	"tool_sep",
}

local function match_ds_template_at(text, pos)
	if text:byte(pos) ~= 60 then -- '<'
		return nil
	end
	local i = pos + 1
	local has_bar = false
	if text:sub(i, i + #FULLWIDTH_BAR - 1) == FULLWIDTH_BAR then
		has_bar = true
		i = i + #FULLWIDTH_BAR
	end
	local key_parts = {}
	local n = #text
	local saw_alpha = false
	while i <= n do
		local b = text:byte(i)
		if (b >= 65 and b <= 90) or (b >= 97 and b <= 122) then
			key_parts[#key_parts + 1] = string.char(b)
			saw_alpha = true
			i = i + 1
		elseif b == 95 then -- '_'
			key_parts[#key_parts + 1] = "_"
			i = i + 1
		elseif text:sub(i, i + #FULLWIDTH_UNDERSCORE - 1) == FULLWIDTH_UNDERSCORE then
			key_parts[#key_parts + 1] = "_"
			i = i + #FULLWIDTH_UNDERSCORE
		else
			break
		end
	end
	if not saw_alpha then
		return nil
	end
	if text:sub(i, i + #FULLWIDTH_BAR - 1) == FULLWIDTH_BAR then
		has_bar = true
		i = i + #FULLWIDTH_BAR
	end
	if not has_bar or text:byte(i) ~= 62 then -- '>'
		return nil
	end
	local key = table.concat(key_parts)
	local canon = DS_TEMPLATE_BY_KEY[key]
	if not canon then
		return nil
	end
	return { token = canon, raw = text:sub(pos, i), index = pos, end_idx = i }
end

local function find_ds_template(text, allowed, from)
	local pos = from or 1
	while true do
		local lt = text:find("<", pos, true)
		if not lt then
			return nil
		end
		local m = match_ds_template_at(text, lt)
		if m and allowed[m.token] then
			return m
		end
		pos = lt + 1
	end
end

local function ds_template_partial_len(text)
	local lt
	for i = #text, math.max(1, #text - 48), -1 do
		if text:byte(i) == 60 then
			lt = i
			break
		end
	end
	if not lt then
		return 0
	end
	local tail = text:sub(lt)
	if tail:find(">", 1, true) then
		return 0
	end
	local i = 2
	if tail:sub(i, i + #FULLWIDTH_BAR - 1) == FULLWIDTH_BAR then
		i = i + #FULLWIDTH_BAR
	elseif #tail >= i and FULLWIDTH_BAR:sub(1, #tail - i + 1) == tail:sub(i) then
		return #tail
	end
	local key_parts = {}
	while i <= #tail do
		local b = tail:byte(i)
		if (b >= 65 and b <= 90) or (b >= 97 and b <= 122) then
			key_parts[#key_parts + 1] = string.char(b)
			i = i + 1
		elseif b == 95 then
			key_parts[#key_parts + 1] = "_"
			i = i + 1
		elseif tail:sub(i, i + #FULLWIDTH_UNDERSCORE - 1) == FULLWIDTH_UNDERSCORE then
			key_parts[#key_parts + 1] = "_"
			i = i + #FULLWIDTH_UNDERSCORE
		elseif FULLWIDTH_UNDERSCORE:sub(1, #tail - i + 1) == tail:sub(i) then
			return #tail
		elseif tail:sub(i, i + #FULLWIDTH_BAR - 1) == FULLWIDTH_BAR then
			local key = table.concat(key_parts)
			return (i + #FULLWIDTH_BAR - 1 == #tail and DS_TEMPLATE_BY_KEY[key] ~= nil) and #tail or 0
		elseif FULLWIDTH_BAR:sub(1, #tail - i + 1) == tail:sub(i) then
			local key = table.concat(key_parts)
			return DS_TEMPLATE_BY_KEY[key] ~= nil and #tail or 0
		else
			return 0
		end
	end
	local key = table.concat(key_parts)
	for _, known in ipairs(DS_TEMPLATE_KEYS) do
		if starts_with(known, key) then
			return #tail
		end
	end
	return 0
end

-------------------------------------------------------------------------------
-- DeepSeek DSML + V3 template streaming scanner
-------------------------------------------------------------------------------

local DS_OUTSIDE_LITERALS = {
	DS_CALLS_BEGIN,
	DS_CALLS_END,
	DS_CALL_BEGIN,
	DSML_OPEN_FW,
	DSML_OPEN_ASCII,
	DSML_CLOSE_FW,
	DSML_CLOSE_ASCII,
	DSML_INVOKE_CLOSE_FW,
	DSML_INVOKE_CLOSE_ASCII,
	DSML_PARAM_CLOSE_FW,
	DSML_PARAM_CLOSE_ASCII,
}

local DS_OUTSIDE_TEMPLATES = {
	[DS_CALLS_BEGIN] = true,
	[DS_CALLS_END] = true,
	[DS_CALL_BEGIN] = true,
}

local DS_ORPHAN_CLOSERS = {
	[DSML_CLOSE_FW] = true,
	[DSML_CLOSE_ASCII] = true,
	[DSML_INVOKE_CLOSE_FW] = true,
	[DSML_INVOKE_CLOSE_ASCII] = true,
	[DSML_PARAM_CLOSE_FW] = true,
	[DSML_PARAM_CLOSE_ASCII] = true,
	[DS_CALLS_END] = true,
}

local DS_SECTION_TOKENS = { DSML_CLOSE_FW, DSML_CLOSE_ASCII, DSML_INVOKE_OPEN_FW, DSML_INVOKE_OPEN_ASCII }
local DS_INVOKE_TOKENS = { DSML_INVOKE_CLOSE_FW, DSML_INVOKE_CLOSE_ASCII, DSML_PARAM_OPEN_FW, DSML_PARAM_OPEN_ASCII }
local DS_PARAM_CLOSERS = { DSML_PARAM_CLOSE_FW, DSML_PARAM_CLOSE_ASCII }
local DS_SEP_ALLOWED = { [DS_SEP] = true }
local DS_ARG_TERMS = { [DS_CALL_END] = true, [DS_CALLS_END] = true, [DS_CALL_BEGIN] = true }

local function make_tool_event(name, args)
	return {
		type = "tool_call",
		name = name,
		arguments = args,
		raw_arguments = mare.json.encode(args),
	}
end

local function new_dsml_scanner()
	local S = {
		buf = "",
		state = "outside",
		in_section = false,
		name = "",
		dsml_args = {},
		param_name = "",
		param_is_str = true,
		param_parts = {},
		pending_section = nil,
		code = { fence = 0, run = 0, newline = false },
		inline_probe = nil,
		wait = new_terminator_wait(),
	}

	local function emit_text(events, text)
		if text ~= "" then
			events[#events + 1] = { type = "text", text = text }
			S.code = advance_code_state(S.code, text)
		end
	end

	local function match_dsml_open(kind)
		local p_fw = kind == "invoke" and DSML_INVOKE_OPEN_FW or DSML_PARAM_OPEN_FW
		local p_as = kind == "invoke" and DSML_INVOKE_OPEN_ASCII or DSML_PARAM_OPEN_ASCII
		if not starts_with(S.buf, p_fw) and not starts_with(S.buf, p_as) then
			return nil
		end
		local gt = S.buf:find(">", 1, true)
		if not gt then
			return nil
		end
		local tag = S.buf:sub(1, gt)
		local name = tag:match('%sname="([^"]*)"')
		if not name then
			return nil
		end
		local str_attr = tag:match('%sstring="(true|false)"') or tag:match('%sstring="(%a+)"')
		S.buf = S.buf:sub(gt + 1)
		return { name = name, is_string = str_attr ~= "false", raw = tag }
	end

	local function find_outside_unquoted(final)
		local search_from = 1
		while true do
			local lit_i, lit_tok = find_earliest_literal(S.buf, DS_OUTSIDE_LITERALS, search_from)
			local tmpl = find_ds_template(S.buf, DS_OUTSIDE_TEMPLATES, search_from)
			local match
			if tmpl and (not lit_i or tmpl.index < lit_i) then
				match = tmpl
			elseif lit_i then
				match = { token = lit_tok, raw = lit_tok, index = lit_i, end_idx = lit_i + #lit_tok - 1 }
			else
				return nil
			end
			local code = advance_code_state(S.code, S.buf:sub(1, match.index - 1))
			local fence = settle_backtick_run(code.fence, code.run, code.fence_char, code.run_char)
			if fence == 0 then
				return match
			end
			if fence < 3 then
				local probe = { fence = fence, run = 0, newline = false }
				local closes = inline_span_closes(probe, S.buf, match.end_idx + 1, final)
				if closes == nil then
					S.inline_probe = probe
					return match.index
				end
				if not closes then
					return match
				end
			end
			search_from = match.end_idx + 1
		end
	end

	local function consume_outside(final, events)
		while S.buf ~= "" do
			if not S.buf:find("<", 1, true) then
				emit_text(events, S.buf)
				S.buf = ""
				return
			end
			local m = find_outside_unquoted(final)
			if type(m) == "number" then
				emit_text(events, S.buf:sub(1, m - 1))
				S.buf = S.buf:sub(m)
				return
			end
			if not m then
				local hold = 0
				if not final then
					hold =
						math.max(partial_suffix_overlap_any(S.buf, DS_OUTSIDE_LITERALS), ds_template_partial_len(S.buf))
				end
				local emit_end = #S.buf - hold
				if emit_end > 0 then
					emit_text(events, S.buf:sub(1, emit_end))
					S.buf = S.buf:sub(emit_end + 1)
				end
				return
			end
			emit_text(events, S.buf:sub(1, m.index - 1))
			S.buf = S.buf:sub(m.end_idx + 1)
			local fence = settle_backtick_run(S.code.fence, S.code.run, S.code.fence_char, S.code.run_char)
			if fence > 0 and fence < 3 then
				S.code = { fence = 0, fence_char = 0, run = 0, run_char = 0, newline = false }
			end
			if m.token == DSML_OPEN_FW or m.token == DSML_OPEN_ASCII then
				S.pending_section = m.raw
				S.state = "dsml_section"
				return
			elseif m.token == DS_CALLS_BEGIN then
				S.in_section = true
				S.state = "v3_section"
				return
			elseif m.token == DS_CALL_BEGIN then
				S.in_section = false
				S.state = "v3_header"
				return
			elseif DS_ORPHAN_CLOSERS[m.token] then
				-- drop orphan closer
			end
		end
	end

	local function consume_dsml_section(final, events)
		local initial = S.buf
		while S.buf ~= "" do
			S.buf = trim_start(S.buf)
			if starts_with(S.buf, DSML_CLOSE_FW) or starts_with(S.buf, DSML_CLOSE_ASCII) then
				local close = starts_with(S.buf, DSML_CLOSE_FW) and DSML_CLOSE_FW or DSML_CLOSE_ASCII
				if S.pending_section ~= nil then
					local consumed_len = #initial - #S.buf
					emit_text(events, S.pending_section .. initial:sub(1, consumed_len) .. close)
					S.pending_section = nil
				end
				S.buf = S.buf:sub(#close + 1)
				S.state = "outside"
				return true
			end
			local inv = match_dsml_open("invoke")
			if inv then
				S.pending_section = nil
				S.name = inv.name
				S.dsml_args = {}
				S.state = "dsml_invoke"
				return true
			end
			if not final then
				if
					(starts_with(S.buf, DSML_INVOKE_OPEN_FW) or starts_with(S.buf, DSML_INVOKE_OPEN_ASCII))
					and not S.buf:find(">", 1, true)
				then
					break
				end
				if partial_suffix_overlap_any(S.buf, DS_SECTION_TOKENS) == #S.buf then
					break
				end
			end
			if S.buf == "" then
				break
			end
			S.buf = S.buf:sub(2)
		end
		if S.pending_section ~= nil then
			local consumed_len = #initial - #S.buf
			S.pending_section = S.pending_section .. initial:sub(1, consumed_len)
		end
		return final
	end

	local function consume_dsml_invoke(final, events)
		while S.buf ~= "" do
			S.buf = trim_start(S.buf)
			if starts_with(S.buf, DSML_INVOKE_CLOSE_FW) or starts_with(S.buf, DSML_INVOKE_CLOSE_ASCII) then
				local close = starts_with(S.buf, DSML_INVOKE_CLOSE_FW) and DSML_INVOKE_CLOSE_FW
					or DSML_INVOKE_CLOSE_ASCII
				S.buf = S.buf:sub(#close + 1)
				events[#events + 1] = make_tool_event(S.name, S.dsml_args)
				S.name = ""
				S.dsml_args = {}
				S.state = "dsml_section"
				return true
			end
			local p = match_dsml_open("parameter")
			if p then
				S.param_name = p.name
				S.param_is_str = p.is_string
				S.param_parts = {}
				S.state = "dsml_param"
				return true
			end
			if not final then
				if
					(starts_with(S.buf, DSML_PARAM_OPEN_FW) or starts_with(S.buf, DSML_PARAM_OPEN_ASCII))
					and not S.buf:find(">", 1, true)
				then
					return false
				end
				if partial_suffix_overlap_any(S.buf, DS_INVOKE_TOKENS) == #S.buf then
					return false
				end
			end
			if S.buf == "" then
				break
			end
			S.buf = S.buf:sub(2)
		end
		return final
	end

	local function consume_dsml_param(final)
		local ci, ctok = find_earliest_literal(S.buf, DS_PARAM_CLOSERS)
		if not ci then
			local hold = final and 0 or partial_suffix_overlap_any(S.buf, DS_PARAM_CLOSERS)
			local take = #S.buf - hold
			if take > 0 then
				S.param_parts[#S.param_parts + 1] = S.buf:sub(1, take)
				S.buf = S.buf:sub(take + 1)
			end
			if final then
				S.state = "outside"
				S.buf = ""
			end
			return false
		end
		if ci > 1 then
			S.param_parts[#S.param_parts + 1] = S.buf:sub(1, ci - 1)
		end
		S.dsml_args[S.param_name] = coerce_dsml_value(table.concat(S.param_parts), S.param_is_str)
		S.buf = S.buf:sub(ci + #ctok)
		S.param_name = ""
		S.param_is_str = true
		S.param_parts = {}
		S.state = "dsml_invoke"
		return true
	end

	local function consume_v3_section(final, events)
		while S.buf ~= "" do
			local rest = trim_start(S.buf)
			local lead = rest ~= "" and match_ds_template_at(rest, 1) or nil
			if lead and lead.token == DS_CALLS_END then
				S.buf = rest:sub(lead.end_idx + 1)
				S.in_section = false
				S.state = "outside"
				return true
			elseif lead and lead.token == DS_CALL_BEGIN then
				S.buf = rest:sub(lead.end_idx + 1)
				S.state = "v3_header"
				return true
			elseif lead then
				S.buf = rest:sub(lead.end_idx + 1)
			elseif rest == "" or ds_template_partial_len(rest) == #rest then
				if not final then
					return false
				end
				S.buf = ""
				return true
			else
				local indent = #S.buf - #rest
				local next_lt = rest:find("<", 2, true)
				local run = next_lt and S.buf:sub(1, indent + next_lt - 1) or S.buf
				local prose = trim_end(run)
				emit_text(events, prose)
				S.buf = S.buf:sub(#prose + 1)
			end
		end
		return final
	end

	local function consume_v3_header(final)
		local sep = find_ds_template(S.buf, DS_SEP_ALLOWED)
		if not sep then
			if final then
				S.buf = ""
				S.state = "outside"
			end
			return false
		end
		local head = trim(S.buf:sub(1, sep.index - 1))
		S.buf = S.buf:sub(sep.end_idx + 1)
		if head == "function" then
			S.state = "v3_legacy_name"
		else
			S.name = head
			S.state = "v3_args"
		end
		return true
	end

	local function consume_v3_legacy_name(final)
		local fence = S.buf:find("```json", 1, true)
		if not fence then
			if final then
				S.buf = ""
				S.state = "outside"
			end
			return false
		end
		S.name = trim(S.buf:sub(1, fence - 1))
		S.buf = S.buf:sub(fence + 7)
		S.state = "v3_legacy_args"
		return true
	end

	local function consume_v3_args(final, events)
		local term = find_ds_template(S.buf, DS_ARG_TERMS)
		if not term then
			if final then
				S.buf = ""
				S.state = "outside"
			end
			return false
		end
		local raw = S.buf:sub(1, term.index - 1)
		if S.state == "v3_legacy_args" then
			local last_fence
			local pos = 1
			while true do
				local fi = raw:find("```", pos, true)
				if not fi then
					break
				end
				last_fence = fi
				pos = fi + 3
			end
			if last_fence then
				raw = raw:sub(1, last_fence - 1)
			end
		end
		local consumed = term.token == DS_CALL_END and term.end_idx or (term.index - 1)
		S.buf = S.buf:sub(consumed + 1)
		events[#events + 1] = make_tool_event(S.name, parse_json_object(raw))
		S.name = ""
		S.state = S.in_section and "v3_section" or "outside"
		return true
	end

	local function consume(final)
		local events = {}
		while S.buf ~= "" do
			if S.state == "outside" then
				consume_outside(final, events)
				if S.state == "outside" then
					break
				end
			elseif S.state == "dsml_section" then
				if not consume_dsml_section(final, events) then
					break
				end
			elseif S.state == "dsml_invoke" then
				if not consume_dsml_invoke(final, events) then
					break
				end
			elseif S.state == "dsml_param" then
				if not consume_dsml_param(final) then
					break
				end
			elseif S.state == "v3_section" then
				if not consume_v3_section(final, events) then
					break
				end
			elseif S.state == "v3_header" then
				if not consume_v3_header(final) then
					break
				end
			elseif S.state == "v3_legacy_name" then
				if not consume_v3_legacy_name(final) then
					break
				end
			else
				if not consume_v3_args(final, events) then
					break
				end
			end
		end
		if final and S.pending_section ~= nil then
			emit_text(events, S.pending_section .. S.buf)
			S.pending_section = nil
			S.buf = ""
			S.state = "outside"
		end
		return events
	end

	function S:feed(chunk)
		if type(chunk) ~= "string" or chunk == "" then
			return {}
		end
		if S.inline_probe then
			S.buf = S.buf .. chunk
			if inline_span_closes(S.inline_probe, chunk, 1, false) == nil then
				return {}
			end
			S.inline_probe = nil
		else
			if S.wait:absorb(chunk) then
				return {}
			end
			S.buf = S.wait:release(S.buf) .. chunk
		end
		local events = consume(false)
		if (S.state == "v3_args" or S.state == "v3_legacy_args") and ds_template_partial_len(S.buf) == 0 then
			S.wait:arm(FULLWIDTH_BAR, S.buf)
		end
		return events
	end

	function S:flush()
		S.inline_probe = nil
		S.buf = S.wait:release(S.buf)
		return consume(true)
	end

	return S
end

-------------------------------------------------------------------------------
-- Kimi section-token streaming scanner
-------------------------------------------------------------------------------

local KIMI_TOKENS = {
	KIMI_SECTION_BEGIN,
	KIMI_SECTION_END,
	KIMI_CALL_BEGIN,
	KIMI_CALL_END,
	KIMI_ARG_BEGIN,
}

local function find_outside_literal_unquoted(S, tokens, final)
	local search_from = 1
	while true do
		local idx, tok = find_earliest_literal(S.buf, tokens, search_from)
		if not idx then
			return nil
		end
		local end_idx = idx + #tok - 1
		local code = advance_code_state(S.code, S.buf:sub(1, idx - 1))
		local fence = settle_backtick_run(code.fence, code.run, code.fence_char, code.run_char)
		if fence == 0 then
			return { index = idx, token = tok, end_idx = end_idx }
		end
		if fence < 3 then
			local probe = { fence = fence, run = 0, newline = false }
			local closes = inline_span_closes(probe, S.buf, end_idx + 1, final)
			if closes == nil then
				return idx
			end
			if not closes then
				return { index = idx, token = tok, end_idx = end_idx }
			end
		end
		search_from = end_idx + 1
	end
end

local function new_kimi_scanner()
	local S = {
		buf = "",
		state = "outside",
		name = "",
		code = { fence = 0, fence_char = 0, run = 0, run_char = 0, newline = false },
		wait = new_terminator_wait(),
	}

	local function emit_text(events, text)
		if text ~= "" then
			events[#events + 1] = { type = "text", text = text }
			S.code = advance_code_state(S.code, text)
		end
	end

	local function consume(final)
		local events = {}
		while S.buf ~= "" do
			if S.state == "outside" then
				local m = find_outside_literal_unquoted(S, KIMI_TOKENS, final)
				if type(m) == "number" then
					emit_text(events, S.buf:sub(1, m - 1))
					S.buf = S.buf:sub(m)
					break
				end
				if not m then
					local hold = final and 0 or partial_suffix_overlap_any(S.buf, KIMI_TOKENS)
					local emit_end = #S.buf - hold
					if emit_end > 0 then
						emit_text(events, S.buf:sub(1, emit_end))
						S.buf = S.buf:sub(emit_end + 1)
					end
					break
				end
				emit_text(events, S.buf:sub(1, m.index - 1))
				S.buf = S.buf:sub(m.end_idx + 1)
				local fence = settle_backtick_run(S.code.fence, S.code.run, S.code.fence_char, S.code.run_char)
				if fence > 0 and fence < 3 then
					S.code = { fence = 0, fence_char = 0, run = 0, run_char = 0, newline = false }
				end
				if m.token == KIMI_SECTION_BEGIN then
					S.state = "section"
				elseif m.token == KIMI_CALL_BEGIN then
					S.state = "header"
				else
					emit_text(events, m.token)
				end
			elseif S.state == "section" then
				S.buf = trim_start(S.buf)
				if S.buf == "" then
					break
				end
				if starts_with(S.buf, KIMI_SECTION_END) then
					S.buf = S.buf:sub(#KIMI_SECTION_END + 1)
					S.state = "outside"
				elseif starts_with(S.buf, KIMI_CALL_BEGIN) then
					S.buf = S.buf:sub(#KIMI_CALL_BEGIN + 1)
					S.state = "header"
				else
					local matched
					for _, t in ipairs(KIMI_TOKENS) do
						if starts_with(S.buf, t) then
							matched = t
							break
						end
					end
					if matched then
						S.buf = S.buf:sub(#matched + 1)
					elseif not final and partial_suffix_overlap_any(S.buf, KIMI_TOKENS) == #S.buf then
						break
					else
						S.buf = S.buf:sub(2)
					end
				end
			elseif S.state == "header" then
				local sep = S.buf:find(KIMI_ARG_BEGIN, 1, true)
				if not sep then
					if final then
						S.buf = ""
						S.state = "outside"
					end
					break
				end
				S.name = normalize_kimi_name(trim(S.buf:sub(1, sep - 1)))
				S.buf = S.buf:sub(sep + #KIMI_ARG_BEGIN)
				S.state = "args"
			else -- args
				local end_i = S.buf:find(KIMI_CALL_END, 1, true)
				if not end_i then
					if final then
						S.buf = ""
						S.state = "outside"
					end
					break
				end
				local raw_args = S.buf:sub(1, end_i - 1)
				if S.name ~= "" then
					events[#events + 1] = make_tool_event(S.name, parse_json_object(raw_args))
				end
				S.name = ""
				S.buf = S.buf:sub(end_i + #KIMI_CALL_END)
				S.state = "section"
			end
		end
		return events
	end

	function S:feed(chunk)
		if type(chunk) ~= "string" or chunk == "" then
			return {}
		end
		if S.wait:absorb(chunk) then
			return {}
		end
		S.buf = S.wait:release(S.buf) .. chunk
		local events = consume(false)
		if S.state == "args" then
			S.wait:arm(KIMI_CALL_END, S.buf)
		end
		return events
	end

	function S:flush()
		S.buf = S.wait:release(S.buf)
		return consume(true)
	end

	return S
end

-------------------------------------------------------------------------------
-- Qwen XML (<tool_calls><fn a="b" /></tool_calls>) + Qwen3 JSON (<tool_call>)
-------------------------------------------------------------------------------

local QWEN_OPEN_TOKENS = { QWEN_XML_OPEN, QWEN3_OPEN }

local function parse_qwen_xml_element(el)
	local name, attr_str = el:match("^<([A-Za-z_][%w%._%-]*)(.-)/%s*>$")
	if not name then
		return nil
	end
	local args = {}
	if attr_str and attr_str ~= "" then
		for k, q, v in attr_str:gmatch("([A-Za-z_][%w%._%-]*)%s*=%s*([\"'])(.-)%2") do
			args[k] = v
		end
	end
	return name, args
end

local function parse_qwen3_json(body)
	local ok, decoded = pcall(mare.json.decode, trim(body))
	if not ok or type(decoded) ~= "table" or type(decoded.name) ~= "string" or decoded.name == "" then
		return nil
	end
	local args = decoded.arguments
	if type(args) == "string" then
		args = parse_json_object(args)
	elseif type(args) ~= "table" then
		args = {}
	end
	return decoded.name, args
end

local function new_qwen_scanner()
	local S = {
		buf = "",
		state = "outside",
		code = { fence = 0, fence_char = 0, run = 0, run_char = 0, newline = false },
		wait = new_terminator_wait(),
	}

	local function emit_text(events, text)
		if text ~= "" then
			events[#events + 1] = { type = "text", text = text }
			S.code = advance_code_state(S.code, text)
		end
	end

	local function consume(final)
		local events = {}
		while S.buf ~= "" do
			if S.state == "outside" then
				local m = find_outside_literal_unquoted(S, QWEN_OPEN_TOKENS, final)
				if type(m) == "number" then
					emit_text(events, S.buf:sub(1, m - 1))
					S.buf = S.buf:sub(m)
					break
				end
				if not m then
					local hold = final and 0 or partial_suffix_overlap_any(S.buf, QWEN_OPEN_TOKENS)
					local emit_end = #S.buf - hold
					if emit_end > 0 then
						emit_text(events, S.buf:sub(1, emit_end))
						S.buf = S.buf:sub(emit_end + 1)
					end
					break
				end
				emit_text(events, S.buf:sub(1, m.index - 1))
				S.buf = S.buf:sub(m.end_idx + 1)
				local fence = settle_backtick_run(S.code.fence, S.code.run, S.code.fence_char, S.code.run_char)
				if fence > 0 and fence < 3 then
					S.code = { fence = 0, fence_char = 0, run = 0, run_char = 0, newline = false }
				end
				S.state = m.token == QWEN_XML_OPEN and "xml_section" or "json_call"
			elseif S.state == "xml_section" then
				local close = S.buf:find(QWEN_XML_CLOSE, 1, true)
				if not close then
					if final then
						S.buf = ""
						S.state = "outside"
					end
					break
				end
				local section = S.buf:sub(1, close - 1)
				for el in section:gmatch("<[^<>]+/>") do
					local name, args = parse_qwen_xml_element(el)
					if name then
						events[#events + 1] = make_tool_event(name, args)
					end
				end
				S.buf = S.buf:sub(close + #QWEN_XML_CLOSE)
				S.state = "outside"
			else -- json_call
				local close = S.buf:find(QWEN3_CLOSE, 1, true)
				if not close then
					if final then
						S.buf = ""
						S.state = "outside"
					end
					break
				end
				local name, args = parse_qwen3_json(S.buf:sub(1, close - 1))
				if name then
					events[#events + 1] = make_tool_event(name, args)
				end
				S.buf = S.buf:sub(close + #QWEN3_CLOSE)
				S.state = "outside"
			end
		end
		return events
	end

	function S:feed(chunk)
		if type(chunk) ~= "string" or chunk == "" then
			return {}
		end
		if S.wait:absorb(chunk) then
			return {}
		end
		S.buf = S.wait:release(S.buf) .. chunk
		local events = consume(false)
		if S.state == "xml_section" then
			S.wait:arm(QWEN_XML_CLOSE, S.buf)
		elseif S.state == "json_call" then
			S.wait:arm(QWEN3_CLOSE, S.buf)
		end
		return events
	end

	function S:flush()
		S.buf = S.wait:release(S.buf)
		return consume(true)
	end

	return S
end

function M.new_scanner(pattern)
	if pattern == "dsml" then
		return new_dsml_scanner()
	elseif pattern == "kimi" then
		return new_kimi_scanner()
	elseif pattern == "qwen" then
		return new_qwen_scanner()
	end
	return nil
end

-------------------------------------------------------------------------------
-- Structured argument tail cutting & replay text/reasoning stripping
-------------------------------------------------------------------------------

local function skip_spaces(s, i)
	while i <= #s do
		local b = s:byte(i)
		if b == 32 or b == 9 or b == 10 or b == 13 then
			i = i + 1
		else
			break
		end
	end
	return i
end

local function consume_pipes(s, i)
	local count = 0
	while count < 2 and i <= #s do
		if s:sub(i, i + #FULLWIDTH_BAR - 1) == FULLWIDTH_BAR then
			i = i + #FULLWIDTH_BAR
			count = count + 1
		elseif s:byte(i) == 124 then -- '|'
			i = i + 1
			count = count + 1
		else
			break
		end
	end
	return count > 0 and i or nil
end

--- Match the start of a DSML tag at `pos` (`<\s*/?\s*[｜|]{1,2}\s*DSML\s*[｜|]`).
--- Returns (is_closing, after_second_pipe_pos) or nil.
local function match_dsml_tag_prefix(s, pos)
	if s:byte(pos) ~= 60 then
		return nil
	end
	local i = skip_spaces(s, pos + 1)
	local closing = false
	if i <= #s and s:byte(i) == 47 then -- '/'
		closing = true
		i = skip_spaces(s, i + 1)
	end
	i = consume_pipes(s, i)
	if not i then
		return nil
	end
	i = skip_spaces(s, i)
	if s:sub(i, i + 3) ~= "DSML" then
		return nil
	end
	i = skip_spaces(s, i + 4)
	i = consume_pipes(s, i)
	if not i then
		return nil
	end
	return closing, i
end

--- Cut leaked DSML tail from a string argument when the tail contains no quotes.
function M.strip_dsml_arg_tail(value)
	if type(value) ~= "string" or not value:find("DSML", 1, true) then
		return nil
	end
	local pos = 1
	while true do
		local lt = value:find("<", pos, true)
		if not lt then
			return nil
		end
		local closing = match_dsml_tag_prefix(value, lt)
		if closing ~= nil then
			local tail = value:sub(lt)
			if tail:find("[\"'`]") then
				return nil
			end
			return value:sub(1, lt - 1)
		end
		pos = lt + 1
	end
end

local function clean_arg_value(val)
	if type(val) == "string" then
		return M.strip_dsml_arg_tail(val) or val
	end
	if type(val) ~= "table" then
		return val
	end
	local out
	for k, v in pairs(val) do
		local cleaned = clean_arg_value(v)
		if cleaned ~= v then
			if not out then
				out = table.copy(val)
			end
			out[k] = cleaned
		end
	end
	return out or val
end

function M.clean_arguments(pattern, args)
	if pattern ~= "dsml" or type(args) ~= "table" then
		return args
	end
	return clean_arg_value(args)
end

local DSML_TAG_NAMES = { tool_calls = true, invoke = true, parameter = true, calls = true }
local DS_ALL_TEMPLATES = {
	[DS_CALLS_BEGIN] = true,
	[DS_CALLS_END] = true,
	[DS_CALL_BEGIN] = true,
	[DS_CALL_END] = true,
	[DS_SEP] = true,
}

local function find_dsml_and_template_tags(seg)
	local tags = {}
	local pos = 1
	while true do
		local lt = seg:find("<", pos, true)
		if not lt then
			break
		end
		local closing, after_pipe = match_dsml_tag_prefix(seg, lt)
		if closing ~= nil then
			local j = skip_spaces(seg, after_pipe)
			local name, after_name = seg:match("^([A-Za-z_]+)()", j)
			if name and DSML_TAG_NAMES[name] then
				local end_idx = seg:find(">", after_name, true)
				if not end_idx then
					local next_lt = seg:find("<", after_name, true)
					end_idx = next_lt and (next_lt - 1) or #seg
				end
				tags[#tags + 1] = { index = lt, end_idx = end_idx, closing = closing }
				pos = end_idx + 1
			else
				pos = lt + 1
			end
		else
			local tmpl = match_ds_template_at(seg, lt)
			if tmpl and DS_ALL_TEMPLATES[tmpl.token] then
				local is_close = tmpl.token == DS_CALL_END or tmpl.token == DS_CALLS_END
				tags[#tags + 1] = { index = lt, end_idx = tmpl.end_idx, closing = is_close }
				pos = tmpl.end_idx + 1
			else
				pos = lt + 1
			end
		end
	end
	return tags
end

local function strip_unquoted_dsml(seg)
	local tags = find_dsml_and_template_tags(seg)
	if #tags == 0 then
		return seg
	end
	local first, last = tags[1], tags[#tags]
	local start_idx = first.index
	local prefix = seg:sub(1, start_idx - 1)
	local lead, name_start, name_tok = prefix:match("^(.*\n)()([ \t]*[A-Za-z_][%w%._%-]*)[ \t]*\r?\n[ \t]*$")
	if lead and trim(name_tok):sub(-1) ~= "." then
		start_idx = name_start
	else
		local only_tok = prefix:match("^[ \t]*([A-Za-z_][%w%._%-]*)[ \t]*\r?\n[ \t]*$")
		if only_tok and only_tok:sub(-1) ~= "." then
			start_idx = 1
		end
	end
	local end_idx = last.end_idx
	if not last.closing then
		local blank = seg:find("\r?\n[ \t]*\r?\n", end_idx + 1)
		end_idx = blank and (blank - 1) or #seg
	end
	local before = trim_end(seg:sub(1, start_idx - 1))
	local after = trim_start(seg:sub(end_idx + 1))
	if before ~= "" and after ~= "" then
		return before .. "\n\n" .. after
	end
	return before .. after
end

local function strip_unquoted_block(seg, open_tok, close_tok)
	local out = seg
	while true do
		local s = out:find(open_tok, 1, true)
		if not s then
			break
		end
		local e = out:find(close_tok, s + #open_tok, true)
		local end_idx = e and (e + #close_tok - 1) or #out
		local before = trim_end(out:sub(1, s - 1))
		local after = trim_start(out:sub(end_idx + 1))
		out = (before ~= "" and after ~= "") and (before .. "\n\n" .. after) or (before .. after)
	end
	return out
end

local function strip_unquoted_for(pattern, seg)
	if pattern == "dsml" then
		return strip_unquoted_dsml(seg)
	elseif pattern == "kimi" then
		local out = strip_unquoted_block(seg, KIMI_SECTION_BEGIN, KIMI_SECTION_END)
		return strip_unquoted_block(out, KIMI_CALL_BEGIN, KIMI_CALL_END)
	elseif pattern == "qwen" then
		local out = strip_unquoted_block(seg, QWEN_XML_OPEN, QWEN_XML_CLOSE)
		return strip_unquoted_block(out, QWEN3_OPEN, QWEN3_CLOSE)
	end
	return seg
end

--- Split `text` into unquoted prose vs Markdown fenced/inline code spans and strip
--- leaked markup only from unquoted stretches.
function M.strip_leaked_text(pattern, text)
	if type(text) ~= "string" or text == "" then
		return text
	end
	if pattern == "dsml" then
		if not text:find("DSML", 1, true) and not text:find(FULLWIDTH_BAR, 1, true) then
			return text
		end
	elseif pattern == "kimi" then
		if not text:find("<|tool_call", 1, true) then
			return text
		end
	elseif pattern == "qwen" then
		if not text:find("<tool_call", 1, true) then
			return text
		end
	else
		return text
	end
	local out = {}
	local cursor = 1
	local n = #text
	while cursor <= n do
		local f_bt = text:find("```", cursor, true)
		local f_ti = text:find("~~~", cursor, true)
		local f_in = text:find("`", cursor, true)
		local q_start, q_end
		if f_bt and (not f_ti or f_bt <= f_ti) and (not f_in or f_bt <= f_in) then
			q_start = f_bt
			local close = text:find("```", f_bt + 3, true)
			q_end = close and (close + 2) or n
		elseif f_ti and (not f_in or f_ti <= f_in) then
			q_start = f_ti
			local close = text:find("~~~", f_ti + 3, true)
			q_end = close and (close + 2) or n
		elseif f_in then
			local nl = text:find("\n", f_in + 1, true)
			local close = text:find("`", f_in + 1, true)
			if close and (not nl or close < nl) then
				q_start, q_end = f_in, close
			else
				out[#out + 1] = strip_unquoted_for(pattern, text:sub(cursor, f_in))
				cursor = f_in + 1
				goto continue_loop
			end
		end
		if not q_start then
			out[#out + 1] = strip_unquoted_for(pattern, text:sub(cursor))
			break
		end
		if q_start > cursor then
			out[#out + 1] = strip_unquoted_for(pattern, text:sub(cursor, q_start - 1))
		end
		out[#out + 1] = text:sub(q_start, q_end)
		cursor = q_end + 1
		::continue_loop::
	end
	return table.concat(out)
end

local function clean_text_parts(pattern, parts)
	if type(parts) ~= "table" then
		return parts
	end
	local out = {}
	for _, p in ipairs(parts) do
		if type(p) == "table" and type(p.text) == "string" then
			local cleaned = M.strip_leaked_text(pattern, p.text)
			if cleaned:match("%S") then
				local copy = table.copy(p)
				copy.text = cleaned
				out[#out + 1] = copy
			end
		else
			out[#out + 1] = p
		end
	end
	return out
end

--- Clean an OpenAI Responses output item (`reasoning`, `message`, `function_call`) for replay.
function M.clean_responses_item(pattern, item)
	if pattern ~= "dsml" or type(item) ~= "table" then
		return item
	end
	if item.type == "reasoning" then
		local copy = table.copy(item)
		if type(copy.summary) == "table" then
			copy.summary = clean_text_parts(pattern, copy.summary)
		end
		if type(copy.content) == "table" then
			copy.content = clean_text_parts(pattern, copy.content)
		end
		return copy
	elseif item.type == "message" and type(item.content) == "table" then
		local copy = table.copy(item)
		copy.content = clean_text_parts(pattern, copy.content)
		return copy
	elseif
		item.type == "function_call"
		and type(item.arguments) == "string"
		and item.arguments:find("DSML", 1, true)
	then
		local ok, parsed = pcall(mare.json.decode, item.arguments)
		if ok and type(parsed) == "table" then
			local cleaned = M.clean_arguments(pattern, parsed)
			if cleaned ~= parsed then
				local copy = table.copy(item)
				copy.arguments = mare.json.encode(cleaned)
				return copy
			end
		end
	end
	return item
end

return M
