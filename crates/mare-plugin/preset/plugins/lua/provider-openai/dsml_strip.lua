-- provider-openai.dsml_strip: lazy DSML argument-tail and Responses replay cleanup for
-- DeepSeek models routed over the OpenAI Responses wire (e.g. OpenRouter Responses).

local M = {}

local FULLWIDTH_BAR = "\239\189\156" -- U+FF5C '｜'
local FULLWIDTH_UNDERSCORE = "\226\150\129" -- U+2581 '▁'

local DSML_TAG_NAMES = { tool_calls = true, invoke = true, parameter = true, calls = true }
local DS_TEMPLATE_CLOSE = { tool_call_end = true, tool_calls_end = true }
local DS_TEMPLATE_KEYS = {
	tool_calls_begin = true,
	tool_calls_end = true,
	tool_call_begin = true,
	tool_call_end = true,
	tool_sep = true,
}

local function trim(s)
	return (s:gsub("^%s+", ""):gsub("%s+$", ""))
end

local function trim_start(s)
	return (s:gsub("^%s+", ""))
end

local function trim_end(s)
	return (s:gsub("%s+$", ""))
end

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
		elseif s:byte(i) == 124 then
			i = i + 1
			count = count + 1
		else
			break
		end
	end
	return count > 0 and i or nil
end

local function match_dsml_tag_prefix(s, pos)
	if s:byte(pos) ~= 60 then
		return nil
	end
	local i = skip_spaces(s, pos + 1)
	local closing = false
	if i <= #s and s:byte(i) == 47 then
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

local function match_ds_template_at(text, pos)
	if text:byte(pos) ~= 60 then
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
		elseif b == 95 then
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
	if not has_bar or text:byte(i) ~= 62 then
		return nil
	end
	local key = table.concat(key_parts)
	if not DS_TEMPLATE_KEYS[key] then
		return nil
	end
	return { key = key, end_idx = i }
end

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

local function strip_unquoted_dsml(seg)
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
			if tmpl then
				tags[#tags + 1] = { index = lt, end_idx = tmpl.end_idx, closing = DS_TEMPLATE_CLOSE[tmpl.key] == true }
				pos = tmpl.end_idx + 1
			else
				pos = lt + 1
			end
		end
	end
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
	return (before ~= "" and after ~= "") and (before .. "\n\n" .. after) or (before .. after)
end

function M.strip_leaked_text(pattern, text)
	if pattern ~= "dsml" or type(text) ~= "string" or text == "" then
		return text
	end
	if not text:find("DSML", 1, true) and not text:find(FULLWIDTH_BAR, 1, true) then
		return text
	end
	local out, cursor, n = {}, 1, #text
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
				out[#out + 1] = strip_unquoted_dsml(text:sub(cursor, f_in))
				cursor = f_in + 1
				goto continue_loop
			end
		end
		if not q_start then
			out[#out + 1] = strip_unquoted_dsml(text:sub(cursor))
			break
		end
		if q_start > cursor then
			out[#out + 1] = strip_unquoted_dsml(text:sub(cursor, q_start - 1))
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
