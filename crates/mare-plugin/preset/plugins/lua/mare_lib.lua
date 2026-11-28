-- mare_lib: small helpers shared by built-in and user plugins, so each doesn't keep its own copy.
-- Owned by `core` (one shared instance per VM in `package.loaded`, loaded once in the core
-- isolated env), so it must stay pure: locals only, nothing beyond what every plugin already has
-- through `mare`. Any plugin may `require` it (typed in types/mare_lib.lua); it adds no `mare.*`
-- API and gives a user plugin nothing it couldn't write itself.

local M = {}

local UTF8_BOM = "\239\187\191"

--- Strip at most one leading UTF-8 BOM (`EF BB BF`) from file text before parsing JSON.
function M.strip_utf8_bom(text)
	if type(text) == "string" and text:sub(1, 3) == UTF8_BOM then
		return text:sub(4)
	end
	return text
end

function M.positive_integer(value)
	return type(value) == "number" and value == value and value ~= math.huge and value > 0 and value % 1 == 0
end

function M.output_limit(requested, model_limit)
	local value = requested
	if value == nil then
		value = model_limit
	end
	if value == nil then
		value = 16384
	end
	return M.positive_integer(value) and value or nil
end

--- Generate a deterministic tool-call ID, advancing the sequence past collisions.
function M.next_hashed_call_id(seen, seed, hex_len, seq)
	while true do
		local id = "call_" .. mare.crypto.sha256(("%s:%d"):format(seed, seq)):sub(1, hex_len)
		if not seen[id] then
			return id, seq
		end
		seq = seq + 1
	end
end

-- Internal parser state: selected fields are lazy byte buffers. Parser state is opaque;
-- final provider messages must be materialized before returning them to callers.
local stream_fields = setmetatable({}, { __mode = "k" })

function M.stream_record(record, keys)
	local fields = {}
	for _, key in ipairs(keys) do
		fields[key] = { value = record[key] }
		record[key] = nil
	end
	stream_fields[record] = fields
	return setmetatable(record, {
		__index = function(_, key)
			local field = fields[key]
			if field then
				if field.buffer and field.value == nil then
					field.value = field.buffer:snapshot()
				end
				return field.value
			end
		end,
		__newindex = function(t, key, value)
			local field = fields[key]
			if field then
				field.value, field.buffer = value, nil
			else
				rawset(t, key, value)
			end
		end,
	})
end

function M.stream_append(record, key, delta, empty_if_nil)
	local field = stream_fields[record][key]
	local value = field.value
	if field.buffer then
		if type(delta) == "string" then
			field.buffer:append(delta)
			field.value = nil
			return
		end
		value = value or field.buffer:snapshot()
	end
	if empty_if_nil then
		value = value or ""
	end
	if type(value) == "string" and type(delta) == "string" then
		local buffer = mare.stream.buffer()
		buffer:append(value)
		buffer:append(delta)
		field.buffer, field.value = buffer, nil
	else
		-- Preserve Lua concatenation errors and numeric coercion on malformed input.
		field.value, field.buffer = value .. delta, nil
	end
end

function M.stream_materialize(record)
	local fields = stream_fields[record]
	if not fields then
		return record
	end
	local out = {}
	for key, value in pairs(record) do
		out[key] = value
	end
	for key in pairs(fields) do
		out[key] = record[key]
	end
	return out
end

--- Tool-call arguments as a table. A parse failure is never silently `{}`: the
--- call keeps `parse_error` + `raw_arguments` and the agent returns the error to the model.
function M.parse_args(raw)
	if type(raw) ~= "string" or raw == "" then
		return nil, "arguments are missing or empty JSON"
	end
	local v, err = mare.json.decode_lossless(raw)
	if type(v) == "table" then
		return v
	end
	return nil, err and tostring(err) or "arguments are not a JSON object"
end

--- Freeze the provider-specific tool declarations before parameter/request hooks. A transport
--- plugin is trusted to send the supplied request; it must not replace its declarations.
function M.guard_tool_declarations(body)
	local tools, functions = body.tools, body.functions
	local function encode(value)
		return value == nil and "<absent>" or mare.json.encode_strict(value)
	end
	local snapshot, legacy_snapshot = encode(tools), encode(functions)
	return function(wire)
		if
			type(wire) ~= "table"
			or type(wire.body) ~= "table"
			or encode(wire.body.tools) ~= snapshot
			or encode(tools) ~= snapshot
			or encode(wire.body.functions) ~= legacy_snapshot
			or encode(functions) ~= legacy_snapshot
		then
			error("provider request changed frozen tool declarations", 0)
		end
		-- Keep the original schema markers, including their property ordering metadata.
		wire.body.tools, wire.body.functions = tools, functions
	end
end

--- `try again in 1.5s` / `in 20ms` / `in 2 seconds` in a rate-limit message → ms, or nil
--- . The patterns are tried in order; the first
--- one giving a number wins.
local RETRY_PATTERNS = {
	"try again in%s*([%d%.]+)%s*(m?s)",
	"try again in%s*(%d+%.?%d*)%s*(m?s)",
	"try again in%s*(%d+%.?%d*)%s*(seconds?)",
}
--- A coded error value for a service contract: `code` is a core code or one the
--- service declares (`<service>.<name>`); it prints as `code: message`, also when the provider
--- calls its own method directly.
--- with_response(llm, resp, fn) -> fn(resp)'s results; the response is closed on every exit
--- (normal end, error, cancel) and errors are re-raised unchanged (drops the stream on each exit
--- path).
function M.with_response(llm, resp, fn)
	local _close <close> = M.guard(function()
		pcall(llm.close_response, llm, resp)
	end)
	return fn(resp)
end

function M.fail(code, message, data)
	error(mare.error(code, message, data), 0)
end

--- Normalize an error string or table into `{ code, message, data? }` for wire events and results.
function M.structured_error(err, code, data)
	if err == nil then
		return nil
	end
	if type(err) == "table" and type(err.code) == "string" and type(err.message) == "string" then
		local out = { code = err.code, message = err.message }
		if err.data ~= nil then
			out.data = err.data
		end
		return out
	end
	local msg = tostring(err)
	if msg == "" then
		return nil
	end
	local out = {
		code = (type(code) == "string" and code ~= "" and code) or "error",
		message = msg,
	}
	if data ~= nil then
		out.data = data
	end
	return out
end

--- Extract the human-readable message from a structured `{ code, message }` error or plain string.
function M.error_message(err)
	if err == nil then
		return nil
	end
	if type(err) == "table" and type(err.message) == "string" then
		return err.message
	end
	return tostring(err)
end

--- Read every custom record of `kind` via paginated `session:list_records` (default branch = "all").
function M.all_records(session, kind, opts)
	if not (session and session.list_records) then
		return {}
	end
	local out, cursor = {}, nil
	local base = type(opts) == "table" and opts or {}
	repeat
		local branch = base.branch or (base.current and "current" or "all")
		local req = {
			branch = branch,
			limit = 200,
			cursor = cursor,
			session_id = base.session_id,
			session_epoch = base.session_epoch,
			loaded_only = type(base.loaded_only) == "boolean" and base.loaded_only or false,
		}
		local ok, page, err = pcall(session.list_records, session, kind, req)
		if not ok then
			return nil, tostring(page)
		end
		if type(page) ~= "table" then
			return nil, err
		end
		for _, item in ipairs(page.data or {}) do
			out[#out + 1] = item
		end
		cursor = page.next_cursor
	until not cursor
	return out
end

function M.retry_after_from_text(message)
	local m = string.lower(tostring(message or ""))
	for _, pat in ipairs(RETRY_PATTERNS) do
		local v, unit = m:match(pat)
		v = tonumber(v)
		if v then
			return unit == "ms" and math.floor(v) or math.floor(v * 1000)
		end
	end
	return nil
end

-- When the prompt fits in the context window and only prompt + max_tokens exceeds it, return
-- the remaining tokens available for the reply.
function M.parse_reply_left(msg)
	local m = string.lower(tostring(msg or ""))
	local window, prompt
	local w1, p1 = m:match("maximum context length is (%d+) tokens.-%((%d+) in the messages, %d+ in the completion%)")
	if w1 and p1 then
		window, prompt = tonumber(w1), tonumber(p1)
	else
		local w2, p2 =
			m:match("maximum context length is (%d+) tokens.-your prompt contains at least (%d+) input tokens")
		if w2 and p2 then
			window, prompt = tonumber(w2), tonumber(p2)
		else
			local p3, w3 =
				m:match("input length and .?max_tokens.? exceed context limit:%s*(%d+)%s*%+%s*%d+%s*>%s*(%d+)")
			if p3 and w3 then
				prompt, window = tonumber(p3), tonumber(w3)
			else
				local w4, inner = m:match(
					"maximum context length is (%d+) tokens%.%s*however,? you requested about %d+ tokens %(([^%)]-%d+ in the output)%)"
				)
				if w4 and inner then
					window = tonumber(w4)
					local sum = 0
					for n in inner:gmatch("(%d+) of [a-z ]+ input") do
						sum = sum + tonumber(n)
					end
					if sum > 0 then
						prompt = sum
					end
				end
			end
		end
	end
	if window and prompt and window > 0 and prompt > 0 and prompt < window then
		return window - prompt
	end
	return nil
end

--- Policy-relevant subset of a tool definition (or call info), copied deeply for command declarations.
function M.tool_policy_info(src)
	src = src or {}
	return {
		side_effect = src.side_effect,
		idempotent = src.idempotent,
		unconfined = src.unconfined,
		permission_request = src.permission_request == true,
		command_tool = src.command_tool == true,
		command = type(src.command) == "table" and M.copy_deep(src.command) or nil,
		access = type(src.access) == "table" and M.copy_deep(src.access) or nil,
		access_state = src.access_state,
		access_version = src.access_version,
		tool_version = src.tool_version,
	}
end

--- Canonical binding of a policy-facing call to its execution declaration. This is data,
--- never an approval token: the caller retains its own authoritative expected binding.
function M.policy_binding(call, info)
	return mare.crypto.sha256(mare.json.encode_strict({
		id = call.id,
		name = call.name,
		cwd = call.cwd,
		arguments = call.arguments,
		snapshot = call.snapshot,
		approval_key = call.approval_key,
		access = call.access,
		info = M.tool_policy_info(info),
	}))
end

--- Expand a `command.argv` placeholder template with `args` and optional `defaults`.
--- Used both at execution time (`tools/command_tools.lua`) and by permission rules (`permissions/rules.lua`).
function M.expand_argv_template(tpl, defaults, args)
	if type(tpl) ~= "table" or type(tpl[1]) ~= "string" or tpl[1] == "" or tpl[1]:find("{", 1, true) then
		return nil, "command template executable must be a literal string"
	end
	args = type(args) == "table" and args or {}
	defaults = type(defaults) == "table" and defaults or nil
	local argv = { tpl[1] }
	for i = 2, #tpl do
		local item = tpl[i]
		if type(item) ~= "string" then
			return nil, "command template item must be a string"
		end
		local only = string.match(item, "^{([%w_]+)}$")
		if only then
			local v = args[only]
			if v == nil and defaults then
				v = defaults[only]
			end
			if v ~= nil then
				local vt = type(v)
				if vt ~= "string" and vt ~= "number" and vt ~= "boolean" then
					return nil, "command template argument is not analyzable"
				end
				argv[#argv + 1] = tostring(v)
			end
		else
			local bad = false
			local replaced = string.gsub(item, "{([%w_]+)}", function(k)
				local v = args[k]
				if v == nil and defaults then
					v = defaults[k]
				end
				if v ~= nil then
					local vt = type(v)
					if vt ~= "string" and vt ~= "number" and vt ~= "boolean" then
						bad = true
						return ""
					end
				end
				return v ~= nil and tostring(v) or ""
			end)
			if bad then
				return nil, "command template argument is not analyzable"
			end
			argv[#argv + 1] = replaced
		end
	end
	return argv
end

--- Recursive copy of a table (non-tables are returned as they are).
function M.copy_deep(t)
	if type(t) ~= "table" then
		return t
	end
	local o = mare and mare.json and mare.json.is_array and mare.json.is_array(t) and mare.json.array() or {}
	for k, v in pairs(t) do
		o[k] = M.copy_deep(v)
	end
	if mare and mare.json and mare.json.is_array and mare.json.is_array(t) then
		return mare.json.array(o)
	end
	return o
end

--- Compare JSON-shaped values without expanding an entire history on the Lua stack.
function M.equal_values(left, right)
	local pending, seen = { { left, right } }, {}
	while #pending > 0 do
		local pair = table.remove(pending)
		local a, b = pair[1], pair[2]
		if a ~= b then
			if type(a) ~= "table" or type(b) ~= "table" or mare.json.is_array(a) ~= mare.json.is_array(b) then
				return false
			end
			seen[a] = seen[a] or {}
			if not seen[a][b] then
				seen[a][b] = true
				for k, v in pairs(a) do
					if b[k] == nil then
						return false
					end
					pending[#pending + 1] = { v, b[k] }
				end
				for k in pairs(b) do
					if a[k] == nil then
						return false
					end
				end
			end
		end
	end
	return true
end

--- Compare message arrays regardless of whether their outer table has a JSON array marker.
function M.equal_messages(left, right)
	if #left ~= #right then
		return false
	end
	for i = 1, #left do
		if not M.equal_values(left[i], right[i]) then
			return false
		end
	end
	return true
end

--- Find exactly one contiguous occurrence of history in messages, in linear comparisons.
function M.find_history(messages, history)
	local n = #history
	if n == 0 or #messages < n then
		return nil
	end
	local prefix, j = { 0 }, 0
	for i = 2, n do
		while j > 0 and not M.equal_values(history[i], history[j + 1]) do
			j = prefix[j]
		end
		if M.equal_values(history[i], history[j + 1]) then
			j = j + 1
		end
		prefix[i] = j
	end
	local found
	j = 0
	for i = 1, #messages do
		while j > 0 and not M.equal_values(messages[i], history[j + 1]) do
			j = prefix[j]
		end
		if M.equal_values(messages[i], history[j + 1]) then
			j = j + 1
		end
		if j == n then
			if found then
				return nil
			end
			found = i - n + 1
			j = prefix[j]
		end
	end
	return found
end

function M.replace_history(messages, start, count, replacement)
	local out = mare.json.array()
	for i = 1, start - 1 do
		out[#out + 1] = messages[i]
	end
	for _, message in ipairs(replacement) do
		out[#out + 1] = message
	end
	for i = start + count, #messages do
		out[#out + 1] = messages[i]
	end
	return out
end

--- `p` relative to the working directory when it lies below it, else `p` unchanged (on Windows
--- whatever the spelling: `\` or `/`, case, `\\?\`; mare.fs.relative).
function M.rel(p)
	return mare.fs.relative(p)
end

--- Read one runtime choice from state.json (the whole state table when `key` is nil); `default`
--- when it is unset. The counterpart of `save_state`. A missing, corrupt or unreadable file is
--- never an error here: it is logged through `ctx` (when given) and the default comes back
--- (`{}` for the whole table), so a plugin starts with its own default.
function M.load_state(ctx, key, default)
	local ok, st, err = pcall(mare.state.read)
	if not ok or type(st) ~= "table" then
		if ctx then
			pcall(ctx.log, ctx, "warn", "state.json: " .. tostring(ok and err or st))
		end
		if key == nil and default == nil then
			return {}
		end
		return default
	end
	if key == nil then
		return st
	end
	local v = st[key]
	if v == nil then
		return default
	end
	return v
end

--- Persist one runtime choice to state.json (mare.state locks, writes atomically and refuses to
--- overwrite a corrupt file). Never from a child process. A failure is logged (mare.log),
--- and with `notify` also shown in the UI when one is connected.
function M.save_state(ctx, key, value, notify)
	if mare.mode == "child" then
		return
	end
	local ok, err = pcall(mare.state.set, key, value)
	if not ok then
		mare.log("warn", "state.json: " .. tostring(err))
		if notify and mare.ui.connected() then
			mare.ui.notify("Not saved: " .. tostring(err), "warning")
		end
	end
end

--- Register a system prompt section owned by a plugin that applies before `context` or doesn't
--- depend on it (tools, permissions): a follower of the `context` service (`ctx:inject({ "context" }, fn)`)
--- — registered now if the `context` service is up and again for each new provider of it;
--- withdrawn with that provider and when the calling plugin is disposed.
function M.prompt_section(ctx, def)
	-- registered while the context service is up, from a follower fiber:
	-- withdrawn with it and registered again for a new provider
	ctx:inject({ "context" }, function(sub)
		sub.context:section(def)
	end)
end

--- The steps' order in llm's readiness chain (`llm:readiness(name, { order = ... })`, ascending;
--- ): the same order whatever the load order and across reloads.
M.READY_ORDER = { llm = 10, mcp = 20, context = 30 }

-- The durable inbox uses complete identified entries; earlier text-only logs are unsupported.
function M.validate_inbox(q)
	assert(type(q) == "table", "invalid session queue")
	local function dense(values)
		assert(type(values) == "table", "invalid session queue array")
		local count = 0
		for key in pairs(values) do
			count = count + 1
			assert(
				type(key) == "number" and key % 1 == 0 and key >= 1 and key <= #values,
				"invalid session queue array"
			)
		end
		assert(count == #values, "invalid session queue array")
	end
	local seen = {}
	for _, target in ipairs({ "steer", "follow_up" }) do
		local entries = q[target]
		if entries ~= nil then
			dense(entries)
			for _, entry in ipairs(entries) do
				assert(
					type(entry) == "table"
						and type(entry.input_id) == "string"
						and entry.input_id ~= ""
						and type(entry.text) == "string",
					"invalid session queue entry"
				)
				assert(not seen[entry.input_id], "duplicate session queue identity")
				seen[entry.input_id] = true
				if entry.images ~= nil then
					dense(entry.images)
				end
				if entry.content ~= nil then
					dense(entry.content)
				end
				if entry.image_markers ~= nil then
					dense(entry.image_markers)
					assert(#entry.image_markers == #(entry.images or {}), "invalid session queue marker count")
					local cursor = 0
					for _, marker in ipairs(entry.image_markers) do
						assert(
							type(marker) == "table"
								and type(marker.text) == "string"
								and marker.text ~= ""
								and type(marker.offset) == "number"
								and marker.offset % 1 == 0
								and marker.offset >= cursor
								and entry.text:sub(marker.offset + 1, marker.offset + #marker.text) == marker.text,
							"invalid session queue marker"
						)
						cursor = marker.offset + #marker.text
					end
				end
			end
		end
	end
	-- `notes`: model notifications waiting for the next request (`agent:notify`)
	if q.notes ~= nil then
		dense(q.notes)
		for _, note in ipairs(q.notes) do
			assert(
				type(note) == "table"
					and type(note.note_id) == "string"
					and note.note_id ~= ""
					and type(note.text) == "string",
				"invalid session queue note"
			)
			assert(not seen[note.note_id], "duplicate session queue identity")
			seen[note.note_id] = true
		end
	end
	return true
end

local function reconcile_marker_offsets(text, safe_text, image_markers, safe_markers)
	if
		type(text) ~= "string"
		or type(safe_text) ~= "string"
		or type(image_markers) ~= "table"
		or type(safe_markers) ~= "table"
	then
		return safe_markers
	end
	local cursor, safe_cursor = 0, 0
	for i, m in ipairs(image_markers) do
		local safe_m = safe_markers[i]
		if
			type(m) == "table"
			and type(safe_m) == "table"
			and type(m.text) == "string"
			and m.text ~= ""
			and type(safe_m.text) == "string"
			and type(m.offset) == "number"
			and m.offset % 1 == 0
			and m.offset >= cursor
			and text:sub(m.offset + 1, m.offset + #m.text) == m.text
		then
			local safe_gap = mare.projection.project(text:sub(cursor + 1, m.offset))
			local new_offset = safe_cursor + #safe_gap
			if safe_text:sub(new_offset + 1, new_offset + #safe_m.text) == safe_m.text then
				safe_m.offset = new_offset
			end
			cursor = m.offset + #m.text
			safe_cursor = safe_m.offset + #safe_m.text
		else
			-- Keep malformed markers invalid so `validate_inbox` continues to reject them.
			break
		end
	end
	return safe_markers
end

--- Project `text` and `image_markers`, recalculating marker byte offsets when projection
--- changes text before a marker.
function M.project_text_and_markers(text, image_markers, array_copy)
	local safe_text = mare.projection.project(text)
	if image_markers == nil then
		return safe_text, nil
	end
	local safe_markers = mare.projection.project(image_markers)
	if array_copy then
		safe_markers = array_copy(safe_markers)
	end
	return safe_text, reconcile_marker_offsets(text, safe_text, image_markers, safe_markers)
end

--- Re-project a canonical queue `inbox` table and keep `image_markers` offsets aligned.
local ATTR_ESCAPES = {
	["&"] = "&amp;",
	["<"] = "&lt;",
	[">"] = "&gt;",
	['"'] = "&quot;",
	["'"] = "&apos;",
	["\n"] = "&#10;",
	["\r"] = "&#13;",
	["\t"] = "&#9;",
}

--- Wrap outside content in a data boundary block `<tag k="v">\nbody\n</tag>`. Attribute values
--- are fully XML-escaped; in the body only this tag's closing form (any case, whitespace allowed)
--- is escaped, so the content stays readable and cannot end the block early. A closing tag in
--- the result is therefore always the runtime's own.
function M.data_block(tag, attrs, body)
	assert(type(tag) == "string" and tag:match("^[%a_][%w_%-%.]*$"), "invalid data block tag")
	local keys = {}
	for key in pairs(attrs or {}) do
		assert(type(key) == "string" and key:match("^[%a_][%w_%-%.]*$"), "invalid data block attribute")
		keys[#keys + 1] = key
	end
	table.sort(keys)
	local open = { "<" .. tag }
	for _, key in ipairs(keys) do
		open[#open + 1] = ('%s="%s"'):format(key, (string.gsub(tostring(attrs[key]), "[&<>\"'\r\n\t]", ATTR_ESCAPES)))
	end
	local text = string.gsub(tostring(body or ""), "\n$", "")
	local lower, tag_lower = string.lower(text), string.lower(tag)
	local pattern = "<%s*/%s*" .. tag_lower:gsub("%p", "%%%0")
	local out, pos = {}, 1
	while true do
		local s, e = string.find(lower, pattern, pos)
		if not s then
			break
		end
		out[#out + 1] = string.sub(text, pos, s - 1) .. "&lt;"
		pos = s + 1
		out[#out + 1] = string.sub(text, pos, e)
		pos = e + 1
	end
	out[#out + 1] = string.sub(text, pos)
	return ("%s>\n%s\n</%s>"):format(table.concat(open, " "), table.concat(out), tag)
end

function M.project_inbox(inbox)
	if type(inbox) ~= "table" then
		return inbox
	end
	local projected = mare.projection.project(inbox)
	if type(projected) ~= "table" then
		return projected
	end
	for _, kind in ipairs({ "steer", "follow_up" }) do
		local raw_list = inbox[kind]
		local safe_list = projected[kind]
		if type(raw_list) == "table" and type(safe_list) == "table" then
			for i, raw_item in ipairs(raw_list) do
				local safe_item = safe_list[i]
				if
					type(raw_item) == "table"
					and type(safe_item) == "table"
					and raw_item.image_markers ~= nil
					and (raw_item.text ~= safe_item.text or raw_item.image_markers ~= safe_item.image_markers)
				then
					safe_item.image_markers = reconcile_marker_offsets(
						raw_item.text,
						safe_item.text,
						raw_item.image_markers,
						safe_item.image_markers
					)
				end
			end
		end
	end
	return projected
end

--- Truncate `s` on a UTF-8 boundary by lines/bytes (`max_lines`), code points (`max_chars`),
--- or bytes (`max_bytes`). Returns `(text, truncated)`.
--- - `max_lines`: delegates to `mare.text.truncate` (`keep = "middle"` by default) and appends
---   the `[output truncated: showing...]` notice when truncated.
--- - `max_chars`: caps to `max_chars` code points, reserving space for `suffix` when truncated
---   or when `force_suffix` is true.
--- - `max_bytes` (without `max_lines`): caps to `max_bytes` bytes via `mare.text.cut`; with
---   `floor_one_char = true` and `max_bytes > 0`, advances at least one UTF-8 character so
---   byte-offset pagination never stalls on a character wider than `max_bytes`.
function M.truncate_utf8(s, opts)
	opts = opts or {}
	if opts.max_lines ~= nil then
		local out, info = mare.text.truncate(s, {
			max_lines = opts.max_lines,
			max_bytes = opts.max_bytes,
			keep = opts.keep or "middle",
			max_line_chars = opts.max_line_chars or 0,
		})
		if info.truncated then
			local h, t, n = info.head_lines or 0, info.tail_lines or 0, info.total_lines
			local which = t > 0 and h > 0 and ("lines 1-%d and %d-%d"):format(h, n - t + 1, n)
				or t > 0 and ("lines %d-%d"):format(n - t + 1, n)
				or ("lines 1-%d"):format(h)
			out = out .. ("\n[output truncated: showing %s of %d]"):format(which, n)
		end
		return out, info.truncated
	end
	if opts.max_chars ~= nil then
		local max_chars = opts.max_chars
		local suffix = opts.suffix
		local over = utf8.offset(s, max_chars + 1) ~= nil
		if suffix then
			if opts.force_suffix or over then
				local suffix_chars = utf8.len(suffix) or #suffix
				if over or utf8.offset(s .. suffix, max_chars + 1) ~= nil then
					return mare.text.cut(s, math.max(0, max_chars - suffix_chars), { unit = "chars" }) .. mare.text.cut(
						suffix,
						max_chars,
						{ unit = "chars" }
					),
						true
				end
				return s .. suffix, true
			end
			return s, false
		end
		if not over then
			return s, false
		end
		return mare.text.cut(s, max_chars, { unit = "chars" }), true
	end
	local max_bytes = opts.max_bytes
	if max_bytes == nil then
		return s, false
	end
	if #s <= max_bytes then
		return s, false
	end
	if max_bytes <= 0 then
		return "", #s > 0
	end
	local capped = mare.text.cut(s, max_bytes)
	if opts.floor_one_char and capped == "" then
		-- First character is wider than max_bytes: advance by one UTF-8 character so pagination never stalls.
		local cut = 1
		while cut < #s and (string.byte(s, cut + 1) or 0) >= 0x80 and (string.byte(s, cut + 1) or 0) < 0xc0 do
			cut = cut + 1
		end
		capped = s:sub(1, cut)
	end
	return capped, #capped < #s
end

--- Planned JSONL file path for a session `id` inside `dir`.
function M.session_file_path(dir, id)
	return ("%s/%s_%s.jsonl"):format(dir, mare.date("%Y-%m-%dT%H-%M-%S"), string.sub(id, -8))
end

--- Sibling `.attachments` directory for a session JSONL `path`.
function M.attachments_dir(path)
	return (path:gsub("%.jsonl$", "")) .. ".attachments"
end

local ESCAPED_UTF8_MARKER = "[mare:escaped-utf8-v1]"

local function has_escaped_marker_prefix(s)
	return type(s) == "string" and s:sub(1, #ESCAPED_UTF8_MARKER) == ESCAPED_UTF8_MARKER
end

local function sanitize_utf8(s)
	if utf8 and utf8.len(s) and not has_escaped_marker_prefix(s) then
		return s
	end
	local out = { ESCAPED_UTF8_MARKER }
	local i, n = 1, #s
	while i <= n do
		local c = s:byte(i)
		if c < 0x80 then
			if c == 0x5C then
				out[#out + 1] = "\\\\"
			else
				out[#out + 1] = string.char(c)
			end
			i = i + 1
		elseif c >= 0xC2 and c <= 0xDF and i + 1 <= n then
			local c2 = s:byte(i + 1)
			if c2 >= 0x80 and c2 <= 0xBF then
				out[#out + 1] = s:sub(i, i + 1)
				i = i + 2
			else
				out[#out + 1] = string.format("\\x%02x", c)
				i = i + 1
			end
		elseif c >= 0xE0 and c <= 0xEF and i + 2 <= n then
			local c2, c3 = s:byte(i + 1, i + 2)
			local valid = c2 >= 0x80 and c2 <= 0xBF and c3 >= 0x80 and c3 <= 0xBF
			if valid and c == 0xE0 and c2 < 0xA0 then
				valid = false
			elseif valid and c == 0xED and c2 > 0x9F then
				valid = false
			end
			if valid then
				out[#out + 1] = s:sub(i, i + 2)
				i = i + 3
			else
				out[#out + 1] = string.format("\\x%02x", c)
				i = i + 1
			end
		elseif c >= 0xF0 and c <= 0xF4 and i + 3 <= n then
			local c2, c3, c4 = s:byte(i + 1, i + 3)
			local valid = c2 >= 0x80 and c2 <= 0xBF and c3 >= 0x80 and c3 <= 0xBF and c4 >= 0x80 and c4 <= 0xBF
			if valid and c == 0xF0 and c2 < 0x90 then
				valid = false
			elseif valid and c == 0xF4 and c2 > 0x8F then
				valid = false
			end
			if valid then
				out[#out + 1] = s:sub(i, i + 3)
				i = i + 4
			else
				out[#out + 1] = string.format("\\x%02x", c)
				i = i + 1
			end
		else
			out[#out + 1] = string.format("\\x%02x", c)
			i = i + 1
		end
	end
	return table.concat(out)
end

local function is_dense_array(t)
	local n = #t
	if n == 0 then
		return false
	end
	for k in pairs(t) do
		if type(k) ~= "number" or k < 1 or k > n or k ~= math.floor(k) then
			return false
		end
	end
	return true
end

local QUARANTINE_MAX_DEPTH = 256

local function validate_json_keys(v, seen, depth)
	if type(v) ~= "table" or seen[v] or depth >= QUARANTINE_MAX_DEPTH then
		return true
	end
	seen[v] = true
	if is_dense_array(v) then
		for i = 1, #v do
			local ok, err = validate_json_keys(v[i], seen, depth + 1)
			if not ok then
				seen[v] = nil
				return false, err
			end
		end
	else
		for k, val in pairs(v) do
			local kt = type(k)
			if kt == "string" then
				if not (utf8 and utf8.len(k)) then
					seen[v] = nil
					return false, "invalid UTF-8 object key"
				end
			elseif kt == "number" then
				if k ~= k or k == math.huge or k == -math.huge then
					seen[v] = nil
					return false, "invalid numeric object key"
				end
			else
				seen[v] = nil
				return false, "invalid object key type: " .. kt
			end
			local ok, err = validate_json_keys(val, seen, depth + 1)
			if not ok then
				seen[v] = nil
				return false, err
			end
		end
	end
	seen[v] = nil
	return true
end

local function sanitize_json_value(v, seen, depth)
	if mare.json and v == mare.json.null then
		return v
	end
	local t = type(v)
	if t == "string" then
		return sanitize_utf8(v)
	elseif t == "number" then
		if v ~= v or v == math.huge or v == -math.huge then
			return (mare.json and mare.json.null) or nil
		end
		return v
	elseif t == "boolean" then
		return v
	elseif t == "table" then
		if seen[v] then
			return "<cycle>"
		end
		if depth >= QUARANTINE_MAX_DEPTH then
			return "<max_depth>"
		end
		seen[v] = true
		local n = #v
		local is_arr = (mare.json and mare.json.is_array and mare.json.is_array(v)) or is_dense_array(v)
		local out = {}
		if is_arr then
			for i = 1, n do
				out[i] = sanitize_json_value(v[i], seen, depth + 1)
			end
			if mare.json and mare.json.array then
				out = mare.json.array(out)
			end
		else
			for k, val in pairs(v) do
				local sk = type(k) == "string" and k or tostring(k)
				out[sk] = sanitize_json_value(val, seen, depth + 1)
			end
		end
		seen[v] = nil
		return out
	else
		return tostring(v)
	end
end

--- Decode a string previously escaped by quarantine UTF-8 sanitization.
function M.unescape_quarantine_utf8(s)
	if not has_escaped_marker_prefix(s) then
		return s
	end
	local out = {}
	local i, n = #ESCAPED_UTF8_MARKER + 1, #s
	while i <= n do
		local c = s:byte(i)
		if c == 0x5C and i + 1 <= n then
			local next_c = s:byte(i + 1)
			if next_c == 0x5C then
				out[#out + 1] = "\\"
				i = i + 2
			elseif next_c == 0x78 and i + 3 <= n then
				local hex = s:sub(i + 2, i + 3)
				if hex:match("^[0-9a-fA-F][0-9a-fA-F]$") then
					out[#out + 1] = string.char(tonumber(hex, 16))
					i = i + 4
				else
					out[#out + 1] = "\\"
					i = i + 1
				end
			else
				out[#out + 1] = "\\"
				i = i + 1
			end
		else
			out[#out + 1] = s:sub(i, i)
			i = i + 1
		end
	end
	return table.concat(out)
end

local function needs_quarantine_sanitization(v, seen, depth)
	local t = type(v)
	if t == "string" then
		return not (utf8 and utf8.len(v)) or has_escaped_marker_prefix(v)
	elseif t ~= "table" or seen[v] or depth >= QUARANTINE_MAX_DEPTH then
		return false
	end
	seen[v] = true
	for _, val in pairs(v) do
		if needs_quarantine_sanitization(val, seen, depth + 1) then
			seen[v] = nil
			return true
		end
	end
	seen[v] = nil
	return false
end

local function encode_quarantine_entry(entry)
	local keys_ok, key_err = validate_json_keys(entry, {}, 0)
	if not keys_ok then
		return nil, key_err
	end
	local line
	if not needs_quarantine_sanitization(entry, {}, 0) then
		local encode_fn = (mare.json and mare.json.encode_strict) or mare.json.encode
		local ok
		ok, line = pcall(encode_fn, entry)
		if ok and type(line) == "string" then
			return line, nil
		end
	end
	local ok_sanitized, sline = pcall(function()
		return mare.json.encode(sanitize_json_value(entry, {}, 0))
	end)
	if ok_sanitized and type(sline) == "string" then
		return sline, nil
	end
	return nil, tostring(sline or line or "quarantine encode failed")
end

--- Unique quarantine JSONL path under `<state_dir>/quarantine/`.
function M.quarantine_path(state_dir, session_id)
	local sid = tostring(session_id or "unknown"):gsub("[^%w%-_]", "_")
	local ts = mare.date("%Y%m%dT%H%M%SZ")
	local suffix = mare.crypto.random_hex(4)
	return ("%s/quarantine/%s-%s-%s.jsonl"):format(state_dir, sid, ts, suffix)
end

--- Append failed session `entries` as JSONL to `path`, anchored to `root_dir` (refusing symlinks).
function M.quarantine_records(path, entries, reason, root_dir)
	if type(path) ~= "string" or path == "" then
		return nil, "missing quarantine path"
	end
	local lines = {}
	for _, entry in ipairs(entries or {}) do
		local line, enc_err = encode_quarantine_entry(entry)
		if not line then
			return nil, enc_err or "quarantine encode failed"
		end
		lines[#lines + 1] = line
	end
	if #lines == 0 then
		return path, nil
	end
	local payload = table.concat(lines, "\n") .. "\n"
	local opts = { append = true, mode = 384 }
	if type(root_dir) == "string" and root_dir ~= "" then
		opts.root = root_dir
	end
	local pcall_ok, w_ok, w_err = pcall(mare.fs.write, path, payload, opts)
	if not pcall_ok then
		return nil, tostring(w_ok)
	end
	if not w_ok then
		return nil, tostring(w_err or "quarantine write failed")
	end
	return path, nil
end

--- Append quarantine status to a persistence error message without duplicating it.
function M.format_save_error(err, quarantine_path, quarantine_error)
	local base = tostring(err or "session write failed")
	local tb = base:find("stack traceback:", 1, true)
	if tb then
		base = base:sub(1, tb - 1):gsub("%s+$", "")
	end
	local qpath_str
	if type(quarantine_path) == "table" then
		local parts = {}
		for _, p in ipairs(quarantine_path) do
			if type(p) == "string" and p ~= "" then
				parts[#parts + 1] = p
			end
		end
		if #parts > 0 then
			qpath_str = table.concat(parts, ", ")
		end
	elseif type(quarantine_path) == "string" and quarantine_path ~= "" then
		qpath_str = quarantine_path
	end
	local has_path = qpath_str ~= nil
	local has_err = quarantine_error and quarantine_error ~= ""
	if has_path and has_err then
		return base .. " (quarantined at " .. qpath_str .. "; quarantine failed: " .. tostring(quarantine_error) .. ")"
	end
	if has_path and not base:find(qpath_str, 1, true) then
		return base .. " (quarantined at " .. qpath_str .. ")"
	end
	if has_err and not base:find("quarantine", 1, true) then
		return base .. " (quarantine failed: " .. tostring(quarantine_error) .. ")"
	end
	return base
end

local function valid_cost(v)
	return type(v) == "number" and v == v and v ~= math.huge and v ~= -math.huge and v >= 0
end

--- True when a dialect/endpoint targets OpenRouter.
function M.is_openrouter(dialect, url)
	if dialect == "openrouter" then
		return true
	end
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
	if authority:sub(1, 1) == "[" then
		return false
	end
	local host = authority:match("^([^:]+):?%d*$")
	if not host or host == "" then
		return false
	end
	host = host:gsub("%.$", "")
	return host == "openrouter.ai" or host:sub(-14) == ".openrouter.ai"
end

--- Compute one request's USD cost and source (`"actual"` | `"estimated"` | `"subscription"` | `"unknown"`).
--- Returns `0, "subscription"` for subscription requests, or `nil, "unknown"` when usage is empty,
--- inconsistent, or pricing is missing for non-zero tokens.
function M.usage_cost(model, u, subscription)
	if subscription then
		return 0, "subscription"
	end
	if type(u) ~= "table" or next(u) == nil then
		return nil, "unknown"
	end
	if valid_cost(u.cost) then
		return u.cost, "actual"
	end
	-- the provider could not tell how its tokens split between prices
	if u.cost_unknown == true then
		return nil, "unknown"
	end
	local c = type(model) == "table" and model.cost or nil
	if type(c) ~= "table" then
		return nil, "unknown"
	end
	local total_cw = u.cache_write or 0
	local raw_1h = u.cache_write_1h or 0
	if raw_1h < 0 or raw_1h > total_cw then
		return nil, "unknown"
	end
	local long_tokens = raw_1h
	local short_tokens = total_cw - long_tokens
	local short_rate = c.cache_write or (c.input and c.input * 1.25)
	-- Fall back to 5m cache_write rate when input rate is omitted so 1h writes are never priced at $0.
	local long_rate = c.cache_write_1h or (c.input and c.input * 2) or short_rate
	if (u.input or 0) > 0 and c.input == nil then
		return nil, "unknown"
	end
	if (u.output or 0) > 0 and c.output == nil then
		return nil, "unknown"
	end
	if (u.cache_read or 0) > 0 and c.cache_read == nil and c.input == nil then
		return nil, "unknown"
	end
	if short_tokens > 0 and short_rate == nil then
		return nil, "unknown"
	end
	if long_tokens > 0 and long_rate == nil then
		return nil, "unknown"
	end
	return (
		(u.input or 0) * (c.input or 0)
		+ (u.output or 0) * (c.output or 0)
		+ (u.cache_read or 0) * (c.cache_read or (c.input or 0) * 0.1)
		+ short_tokens * (short_rate or 0)
		+ long_tokens * (long_rate or 0)
	) / 1e6,
		"estimated"
end

--- Provider-reported billed cost from OpenRouter `usage` accounting.
--- Returns nil when accounting is absent or incomplete so callers fall back to catalog pricing.
function M.openrouter_cost(u)
	if type(u) ~= "table" or not valid_cost(u.cost) then
		return nil
	end
	if u.is_byok == false then
		return u.cost
	end
	if u.is_byok == true then
		local d = u.cost_details
		if type(d) ~= "table" then
			return nil
		end
		local upstream = d.upstream_inference_cost
		if upstream == nil or upstream == mare.json.null then
			if
				not valid_cost(d.upstream_inference_prompt_cost)
				or not valid_cost(d.upstream_inference_completions_cost)
			then
				return nil
			end
			upstream = d.upstream_inference_prompt_cost + d.upstream_inference_completions_cost
		end
		if not valid_cost(upstream) then
			return nil
		end
		return u.cost + upstream
	end
	return nil
end

--- Derive a model's image capability (`true`, `false`, or `nil` when unknown) from
--- `m.input` (a non-empty list yields `true` iff `"image"` is present and `false`
--- otherwise). Detached read-only descriptors without `input` fall back to boolean
--- `m.images`. Empty or absent `input` keeps `nil` (unknown).
function M.model_images(m)
	if type(m) ~= "table" then
		return nil
	end
	local mods = type(m.input) == "table" and m.input or nil
	if type(mods) == "table" and #mods > 0 then
		for _, v in ipairs(mods) do
			if v == "image" then
				return true
			end
		end
		return false
	end
	if type(m.images) == "boolean" then
		return m.images
	end
	return nil
end

local CACHE_RETENTION_TIERS = { none = true, short = true, long = true }

--- Validate a cache_retention value ("none" | "short" | "long").
function M.validate_cache_retention(val, source)
	if val == nil then
		return nil
	end
	if type(val) == "string" and CACHE_RETENTION_TIERS[val] then
		return val
	end
	local prefix = source and (tostring(source) .. ": ") or ""
	error({
		message = prefix .. "invalid cache_retention: " .. tostring(val) .. " (expected none, short, or long)",
		kind = "invalid",
		retryable = false,
	}, 0)
end

local function first_non_nil(...)
	for i = 1, select("#", ...) do
		local v = select(i, ...)
		if v ~= nil then
			return v
		end
	end
	return nil
end

--- Resolve a boolean/string capability flag across model -> provider -> route -> plugin -> default.
--- Explicit `false` is preserved and never skipped.
function M.resolve_cache_cap(key, default_val, model, prov_cfg, route_cfg, plugin_cfg)
	local layers = { model, prov_cfg, route_cfg, plugin_cfg }
	for i = 1, 4 do
		local t = layers[i]
		if type(t) == "table" and t[key] ~= nil then
			return t[key]
		end
	end
	return default_val
end

--- Emit a caller-visible warning when a requested cache_retention tier cannot be guaranteed.
function M.warn_cache_limitation(ctx, msg)
	mare.log("warn", msg)
	if mare and mare.ui and mare.ui.notify then
		pcall(mare.ui.notify, msg, "warning")
	end
end

--- Resolve effective prompt cache retention ("none" | "short" | "long"), read_only flag,
--- supports_long flag, and resolution source across request, model, provider, route/endpoint,
--- plugin, and global layers.
function M.resolve_cache_retention(req, model, prov_cfg, plugin_cfg, route_cfg, global_cfg)
	local read_only = type(req) == "table" and req.cache == "read"
	if type(req) == "table" and req.cache == false then
		return "none", false, false, nil
	end
	local tier, source
	local global_retention
	if type(global_cfg) == "table" then
		global_retention = global_cfg.cache_retention
	else
		global_retention = mare and mare.config and mare.config.cache_retention
	end
	local candidates = {
		{ "request", type(req) == "table" and M.validate_cache_retention(req.cache_retention, "request") or nil },
		{ "model", type(model) == "table" and M.validate_cache_retention(model.cache_retention, "model") or nil },
		{
			"provider",
			type(prov_cfg) == "table" and M.validate_cache_retention(prov_cfg.cache_retention, "provider") or nil,
		},
		{
			"route",
			type(route_cfg) == "table" and M.validate_cache_retention(route_cfg.cache_retention, "route") or nil,
		},
		{
			"plugin",
			type(plugin_cfg) == "table" and M.validate_cache_retention(plugin_cfg.cache_retention, "plugin") or nil,
		},
		{ "config", M.validate_cache_retention(global_retention, "config") },
	}
	for i = 1, #candidates do
		if candidates[i][2] ~= nil then
			source, tier = candidates[i][1], candidates[i][2]
			break
		end
	end
	if tier == nil then
		tier, source = "short", "default"
	end
	local supports_long =
		M.resolve_cache_cap("supports_long_cache_retention", true, model, prov_cfg, route_cfg, plugin_cfg)
	return tier, read_only, supports_long == true, source
end

--- Build an Anthropic cache_control marker table (or nil when caching is disabled / read-only).
function M.anthropic_cache_control(retention, supports_long, read_only)
	if read_only or retention == "none" then
		return nil
	end
	if retention == "long" and supports_long then
		return { type = "ephemeral", ttl = "1h" }
	end
	return { type = "ephemeral" }
end

--- True when the last assistant message's provider-reported usage on `path`/`entries` is
--- stale (predates the latest compaction on the branch, a projection/prune of its active
--- context, or an in-memory compaction during load).
function M.path_usage_stale(path, entries, compacted)
	if compacted then
		return true
	end
	if type(path) ~= "table" or #path == 0 then
		return false
	end
	local cmp_idx, bs_idx, ast_idx
	for i = #path, 1, -1 do
		local e = path[i]
		if not cmp_idx and e.type == "compaction" then
			cmp_idx = i
		elseif not bs_idx and e.type == "branch_summary" then
			bs_idx = i
		elseif
			not ast_idx
			and e.type == "message"
			and e.message
			and e.message.role == "assistant"
			and type(e.message.usage) == "table"
		then
			ast_idx = i
		end
		if cmp_idx and ast_idx then
			break
		end
	end
	if cmp_idx and (not ast_idx or ast_idx < cmp_idx) then
		return true
	end
	if bs_idx and (not ast_idx or ast_idx < bs_idx) then
		return true
	end
	if not ast_idx then
		return false
	end
	local start_idx = 1
	if cmp_idx then
		local first_kept = path[cmp_idx].first_kept_entry_id
		start_idx = cmp_idx + 1
		if first_kept then
			for j = 1, cmp_idx do
				if path[j].entry_id == first_kept then
					start_idx = j
					break
				end
			end
		end
	end
	local active_ids = {}
	for j = start_idx, #path do
		if path[j].entry_id then
			active_ids[path[j].entry_id] = true
		end
	end
	local last_ast_id = path[ast_idx].entry_id
	if type(entries) ~= "table" then
		return false
	end
	for i = #entries, 1, -1 do
		local e = entries[i]
		if last_ast_id and e.entry_id == last_ast_id then
			return false
		end
		if e.type == "prune" and type(e.target_entry_ids) == "table" then
			for id in pairs(e.target_entry_ids) do
				if active_ids[id] then
					return true
				end
			end
		end
	end
	return false
end

--- Local token estimate of one message (remote/opaque items count as their serialized form).
function M.message_tokens(m)
	if type(m.remote) == "table" and type(m.remote.items) == "table" then
		return mare.text.tokens(m.remote.items)
	end
	return mare.text.tokens(m.content or {})
end

--- Local token estimate of a message list: the fallback context size when provider usage is
--- unknown or stale (e.g. resumed after a compaction/prune with no compact plugin loaded).
function M.estimate_messages(msgs)
	local n = 0
	for _, m in ipairs(msgs or {}) do
		n = n + M.message_tokens(m)
	end
	return n
end

--- True when the last assistant message's provider-reported usage in a loaded session is
--- stale (predates the latest compaction on the branch, a projection/prune of its active
--- context, or an in-memory compaction during load). Prefers precomputed `ev.usage_stale`
--- when present so partially-loaded sessions are not expanded on `session/loaded`.
function M.session_usage_stale(session, ev)
	if ev and ev.compacted then
		return true
	end
	if ev and type(ev.usage_stale) == "boolean" then
		return ev.usage_stale
	end
	if not session or not session.path_to or not session.entries then
		return false
	end
	local ok_p, path = pcall(session.path_to, session)
	if not ok_p or type(path) ~= "table" or #path == 0 then
		return false
	end
	local ok_e, entries = pcall(session.entries, session)
	if not ok_e then
		return false
	end
	return M.path_usage_stale(path, entries, false)
end

--- Publish `impl` as the mare.Service `name`: `impl` is a table of methods
--- written `function(impl, caller, ...)`; the service's methods call them with the caller's ctx
--- (`self.ctx`, the traced caller) as `caller`, so a registration a method makes with
--- `caller:effect(...)` is an effect of the caller's fiber. The published value is a traceable
--- mare.Service instance; `impl` keeps working for the
--- plugin's own calls. Every method must be on `impl` when this runs. Returns the instance.
---@param ctx table the plugin's ctx (the legacy layer's or a cordis ctx)
---@param name string
---@param impl table
---@return table
function M.service(ctx, name, impl)
	local cordis = require("cordis")
	local Class = cordis.class(name, mare.Service)
	function Class:constructor(owner)
		Class.super.constructor(self, owner, name)
	end
	-- the class's own constructor, taken before the methods are copied on: a service method may be
	-- called `new` (session.new) and then shadows `Class.new` (cordis itself
	-- constructs components through class.construct)
	local construct = Class.new
	for k, v in pairs(impl) do
		if type(v) == "function" then
			Class[k] = function(self, ...)
				return v(impl, self.ctx, ...)
			end
		else
			Class[k] = v
		end
	end
	local svc = construct(ctx)
	-- One method table, as when the published table was the implementation: a method replaced on
	-- the published service (`svc.record = fn`, a test seam or a wrapping plugin) is also what the
	-- provider's own calls (`impl.record(impl, caller, ...)`) run. Until then they run the original,
	-- unguarded, as before.
	for k, v in pairs(impl) do
		if type(v) == "function" then
			local published = rawget(svc, k)
			impl[k] = function(self, caller, ...)
				local current = rawget(svc, k)
				if current ~= nil and current ~= published then
					return current(svc, ...)
				end
				return v(self, caller, ...)
			end
		end
	end
	return svc
end

--- The plugin a caller ctx belongs to: its fiber name, else nil (init.lua, the host).
---@param caller table|nil
---@return string|nil
function M.caller_name(caller)
	if caller == nil then
		return nil
	end
	local ok, n = pcall(function()
		return caller.fiber.name
	end)
	return ok and type(n) == "string" and n or nil
end

--- The RPC connection id if `caller` (or an ancestor fiber) belongs to an active RPC connection.
---@param caller any
---@return any|nil
function M.rpc_caller_id(caller)
	if type(caller) ~= "table" then
		return nil
	end
	local ok, id = pcall(function()
		local rpc = caller:get("rpc")
		if not rpc or not rpc.ids then
			return nil
		end
		local f = caller.fiber
		while f do
			local conn_id = rpc.ids[f]
			if conn_id ~= nil then
				return conn_id
			end
			local parent = f.parent and f.parent.fiber
			if parent == f then
				break
			end
			f = parent
		end
		return nil
	end)
	return ok and id or nil
end

--- Follow the optional service `name`: set up in `apply`,
--- returns a getter for the service while it is up, nil otherwise. The getter reads the active
--- provider on each call (`ctx:get(name)`: strict, only while the provider's fiber is ACTIVE), as
--- cordis reads an optional service at the point of use (upstream reflect.ts `get`, the loader's
--- config/tree.ts). So a read in `apply` already sees a provider that is up, where an inject
--- follower would start one job hop later; and a service that goes or is replaced is seen at once.
--- The getter returns nil once the plugin itself is unloaded. The plugin does not depend on the
--- service. For reads made on each call; a registration into an optional service goes in its own
--- `ctx:inject({ name }, fn)` follower instead (what it registers then goes with the service).
---@param ctx table the plugin's ctx (in `apply`)
---@param name string
---@return fun(): table|nil
function M.follow(ctx, name)
	local gone = false
	ctx:effect(function()
		return function()
			gone = true
		end
	end, "follow " .. name)
	return function()
		if gone then
			return nil
		end
		return ctx:get(name)
	end
end

--- Follow several optional services at once (sharing a single teardown effect): returns a table
--- whose field `<name>` reads the active service, nil while it is not up or after unload. Set up
--- once in `apply`; reads on each call (`svc.session`, not a cached local), also inside `apply`.
---@param ctx table the plugin's ctx (in `apply`)
---@param names string[]
---@return table<string, table|nil>
function M.followers(ctx, names)
	local allowed = {}
	for i = 1, #names do
		allowed[names[i]] = true
	end
	local gone = false
	ctx:effect(function()
		return function()
			gone = true
		end
	end, "followers")
	return setmetatable({}, {
		__index = function(_, name)
			if gone or not allowed[name] then
				return nil
			end
			return ctx:get(name)
		end,
		__newindex = function()
			error("followed services are read-only", 2)
		end,
	})
end

--- Whether `v` can be called: a function, or a callable table. A service's method read through
--- the tracking proxy of a cordis Service is a callable table, not a function, so
--- an optional-method check on a service uses this instead of `type(v) == "function"`.
---@param v any
---@return boolean
function M.callable(v)
	if type(v) == "function" then
		return true
	end
	if type(v) ~= "table" then
		return false
	end
	local ok, cordis = pcall(require, "cordis")
	if ok then
		return cordis.proto.callable(v)
	end
	local mt = getmetatable(v)
	return type(mt) == "table" and rawget(mt, "__call") ~= nil
end

--- A stable identity for the plugin a caller ctx belongs to, for per-owner bookkeeping (e.g.
--- sandbox cells, presentation documents): its fiber's `uid` (unique while the fiber lives),
--- else nil (init.lua, the host).
---@param caller table|userdata|nil
---@return integer|nil
function M.caller_id(caller)
	if caller == nil then
		return nil
	end
	local ok, id = pcall(function()
		return caller.fiber.uid
	end)
	return ok and type(id) == "number" and id or nil
end

--- Tie `cleanup` to the caller's fiber: a mare.Service method passes the caller's
--- cordis ctx (`self.ctx`). Returns the disposer to call for an early withdrawal (it ends the effect
--- too). Without a caller (a JSON caller through the bridge) `cleanup` is returned as it is.
---@param caller table|nil
---@param cleanup fun(): any
---@param label? string
---@return fun(): any
function M.owned(caller, cleanup, label)
	if type(caller) == "table" and caller.effect then
		local dispose = caller:effect(function()
			return cleanup
		end, label)
		return function()
			return dispose()
		end
	end
	return cleanup
end

--- Manage per-caller owner buckets tied to both the caller's fiber (`caller:effect`) and the
--- providing service's own lifetime (`ctx:effect`), used by stateful services like `proc` and `terminal`.
---@param ctx table
---@param close_fn fun(bucket: any)
---@param label string
---@return table
function M.owner_registry(ctx, close_fn, label)
	local owners = {}
	local function close_owner(owner_id)
		local bucket = owners[owner_id]
		if not bucket then
			return
		end
		owners[owner_id] = nil
		close_fn(bucket)
	end
	local function close_all()
		local ids = {}
		for id in pairs(owners) do
			ids[#ids + 1] = id
		end
		for _, id in ipairs(ids) do
			close_owner(id)
		end
	end
	ctx:effect(function()
		return close_all
	end, label .. " service")
	return {
		get = function(caller, create_fn)
			local id = M.caller_id(caller)
			assert(type(id) == "number", label .. " handle requires an owner context")
			local bucket = owners[id]
			if not bucket and create_fn then
				bucket = create_fn(id)
				owners[id] = bucket
				caller:effect(function()
					return function()
						close_owner(id)
					end
				end, label .. " owner")
			end
			return bucket, id
		end,
		close_all = close_all,
	}
end

--- An ordered, removable list (the ordered chains: ui:handler, llm:readiness): entries run in
--- ascending `order`, equal orders in the order they were added. Traversals work on a snapshot,
--- so an entry may add or remove entries while one runs.
---   list.add(order, value) -> remove (idempotent)
---   list.first(fn) -> the first truthy fn(value), or nil: stops there
---   list.all(fn) -> whether any fn(value) was truthy: runs every entry
function M.ordered()
	local entries, seq = {}, 0
	local list = {}
	function list.add(order, value)
		assert(type(order) == "number", "an ordered entry needs a numeric order")
		seq = seq + 1
		local entry = { order = order, seq = seq, value = value }
		entries[#entries + 1] = entry
		return function()
			local i = table.find(entries, entry)
			if i then
				table.remove(entries, i)
			end
		end
	end
	local function snapshot()
		local out = table.move(entries, 1, #entries, 1, {})
		table.sort(out, function(a, b)
			return a.order < b.order or (a.order == b.order and a.seq < b.seq)
		end)
		return out
	end
	function list.first(fn)
		for _, e in ipairs(snapshot()) do
			local r = fn(e.value)
			if r then
				return r
			end
		end
		return nil
	end
	function list.all(fn)
		local any = false
		for _, e in ipairs(snapshot()) do
			any = fn(e.value) and true or any
		end
		return any
	end
	return list
end

--- True when `path` is project-relative (not `~...`, `/...`, `\\...` on Windows, or a drive path `X:\`/`X:/`).
function M.is_project_relative(path)
	if type(path) ~= "string" or path == "" then
		return false
	end
	if path:match("^~") or path:match("^/") or path:match("^%a:[/\\]") then
		return false
	end
	if mare.os == "windows" and path:match("^\\") then
		return false
	end
	return true
end

local function mare_dir_rejected(root)
	if mare.project and mare.project.rejected then
		for _, r in ipairs(mare.project.rejected) do
			if r == root .. "/.mare" or r == root .. "\\.mare" then
				return true
			end
		end
	end
	return false
end

--- Resolve `rel` inside `mare.project.root`, returning nil when no project resource root
--- exists or when `rel` targets a rejected `.mare` symlink.
function M.project_file(rel)
	local root = mare.project and mare.project.root
	if type(root) ~= "string" or root == "" or type(rel) ~= "string" or rel == "" then
		return nil
	end
	local norm = rel:gsub("^%.[/\\]", "")
	if (norm == ".mare" or norm:match("^%.mare[/\\]")) and mare_dir_rejected(root) then
		return nil
	end
	if norm:match("^%.mare[/\\]") then
		local entry = mare.fs.stat(root .. "/.mare")
		if entry and entry.kind == "file" then
			return nil
		end
	end
	return root .. "/" .. rel
end

--- Resolve a directory option entry: project-relative entries resolve inside `mare.project.root`
--- (or nil when no root / rejected `.mare`), while `~`/absolute entries resolve via `mare.fs.resolve`.
function M.resolve_project_dir(entry)
	if type(entry) ~= "string" or entry == "" then
		return nil
	end
	if M.is_project_relative(entry) then
		return M.project_file(entry)
	end
	return mare.fs.resolve(entry)
end

--- Shared default `model_defaults` for OpenAI-compatible/gateway providers that route GPT and
--- Codex models through `apply_patch` and `gpt_rules`.
M.APPLY_PATCH_MODEL_DEFAULTS = {
	["*gpt*"] = { edit_tool = "apply_patch", gpt_rules = true },
	["*codex*"] = { edit_tool = "apply_patch", gpt_rules = true },
}

local function is_json_array(t)
	if type(t) ~= "table" then
		return false
	end
	if mare.json and type(mare.json.is_array) == "function" then
		return mare.json.is_array(t)
	end
	local n = #t
	local count = 0
	for k in pairs(t) do
		if type(k) ~= "number" or k < 1 or k > n or k % 1 ~= 0 then
			return false
		end
		count = count + 1
	end
	return count == n
end

local function has_error_field(t)
	local err = type(t) == "table" and t.error or nil
	local json_null = mare and mare.json and mare.json.null
	return err ~= nil and err ~= json_null and err ~= false
end

M.is_json_array = is_json_array
M.has_error_field = has_error_field

--- True (plus element count) when `value` is a table whose keys are dense integers `1..n` (`0..n`).
function M.is_dense_list(value)
	if type(value) ~= "table" then
		return false, 0
	end
	local count, max = 0, 0
	for k in pairs(value) do
		if type(k) ~= "number" or k < 1 or k >= math.huge or k % 1 ~= 0 then
			return false, 0
		end
		count, max = count + 1, math.max(max, k)
	end
	if count ~= max then
		return false, 0
	end
	return true, count
end

--- Decode and validate a provider model-listing JSON envelope (`{ [field] = [ { [id_field] = "..." }, ... ] }`).
--- Throws on malformed JSON, error payloads, non-array lists, or invalid items so callers keep the cache.
function M.decode_models_envelope(raw, field, id_field, label)
	local ok, lossless = pcall(mare.json.decode_lossless, raw)
	if
		not ok
		or type(lossless) ~= "table"
		or is_json_array(lossless)
		or has_error_field(lossless)
		or not is_json_array(lossless[field])
	then
		error(("%s: invalid response"):format(label), 0)
	end
	for _, m in ipairs(lossless[field]) do
		if
			type(m) ~= "table"
			or is_json_array(m)
			or has_error_field(m)
			or type(m[id_field]) ~= "string"
			or m[id_field] == ""
		then
			error(("%s: invalid response"):format(label), 0)
		end
	end
	local j = mare.json.decode(raw)
	if type(j) ~= "table" or type(j[field]) ~= "table" then
		error(("%s: invalid response"):format(label), 0)
	end
	return j[field], lossless[field], j
end

--- Slice `items` by `{ cursor?, limit? }` with strict positive-integer validation.
function M.paginate(items, opts)
	opts = type(opts) == "table" and opts or {}
	local start_idx = 1
	if opts.cursor ~= nil then
		if type(opts.cursor) ~= "string" or not opts.cursor:match("^%d+$") then
			M.fail("invalid_arguments", "invalid cursor: " .. tostring(opts.cursor))
		end
		local parsed = tonumber(opts.cursor)
		if not parsed or parsed < 1 or parsed >= math.huge or math.floor(parsed) ~= parsed then
			M.fail("invalid_arguments", "invalid cursor: " .. tostring(opts.cursor))
		end
		start_idx = parsed
	end
	local limit = opts.limit
	if limit ~= nil and (type(limit) ~= "number" or limit < 1 or limit >= math.huge or math.floor(limit) ~= limit) then
		M.fail("invalid_arguments", "limit must be a positive integer")
	end
	local data = {}
	local end_idx = limit and math.min(#items, start_idx + limit - 1) or #items
	for i = start_idx, end_idx do
		data[#data + 1] = items[i]
	end
	local next_cursor = end_idx < #items and tostring(end_idx + 1) or nil
	return { data = mare.json.array(data), next_cursor = next_cursor }
end

local function strip_json_nulls(value, seen)
	local json_null = mare.json and mare.json.null
	if value == nil or (json_null ~= nil and value == json_null) then
		return nil
	end
	if type(value) ~= "table" then
		return value
	end
	seen = seen or {}
	if seen[value] then
		error("cyclic render node", 0)
	end
	seen[value] = true
	local out = {}
	local is_array = mare.json and mare.json.is_array and mare.json.is_array(value)
	for k, v in pairs(value) do
		if json_null == nil or v ~= json_null then
			local cleaned = strip_json_nulls(v, seen)
			if cleaned ~= nil then
				out[k] = cleaned
			end
		end
	end
	seen[value] = nil
	if is_array and mare.json and mare.json.array then
		return mare.json.array(out)
	end
	return out
end

--- Normalize a custom message or entry renderer result: unwrap `{node}` / treat `json.null` or `{}`
--- as `nil`, recursively strip `json.null` optional fields, project secrets, optionally decode wire
--- images via `node_from_wire`, and validate via `mare.ui.validate_node`.
function M.normalize_render_result(res, opts)
	local json_null = mare.json and mare.json.null
	if res == nil or (json_null ~= nil and res == json_null) then
		return nil
	end
	if type(res) == "table" then
		if res.node ~= nil then
			if json_null ~= nil and res.node == json_null then
				return nil
			end
			res = res.node
		elseif next(res) == nil then
			return nil
		end
	end
	res = strip_json_nulls(res)
	if res == nil then
		return nil
	end
	if mare.projection and mare.projection.project then
		res = mare.projection.project(res, { preserve_invalid = true })
	end
	if opts and opts.wire and mare.ui and mare.ui.node_from_wire then
		res = mare.ui.node_from_wire(res)
	end
	if mare.ui and mare.ui.validate_node then
		local ok, info = mare.ui.validate_node(res)
		if not ok then
			error(type(info) == "string" and info or "invalid render node", 0)
		end
	end
	return res
end

--- Resolve whether a chat/responses wire should inject placeholder reasoning on assistant
--- tool-call turns. Precedence:
---   1. user/explicit model `needs_reasoning_placeholder`
---   2. user provider override (`mare.config.providers[p].needs_reasoning_placeholder`)
---   3. models.dev `interleaved` enrichment (`sources.needs_reasoning_placeholder == "models_dev"`)
---   4. preset declaration (`needs_reasoning_placeholder`)
---   5. dialect/host substring fallback
function M.needs_reasoning_placeholder(model, ep, fallback, sources)
	local m_val, m_src
	if type(model) == "table" then
		m_val = model.needs_reasoning_placeholder
		m_src = (type(sources) == "table" and sources.needs_reasoning_placeholder) or nil
	end
	if m_val ~= nil and m_src ~= "models_dev" then
		return m_val == true
	end
	local user_cfg = type(ep) == "table" and ep.user_provider_config or nil
	if type(user_cfg) == "table" and user_cfg.needs_reasoning_placeholder ~= nil then
		return user_cfg.needs_reasoning_placeholder == true
	end
	if m_val ~= nil then
		return m_val == true
	end
	local preset_cfg = type(ep) == "table" and ep.preset_config or nil
	if type(preset_cfg) == "table" and preset_cfg.needs_reasoning_placeholder ~= nil then
		return preset_cfg.needs_reasoning_placeholder == true
	end
	if user_cfg == nil and preset_cfg == nil then
		local prov_cfg = type(ep) == "table" and ep.provider_config or nil
		if type(prov_cfg) == "table" and prov_cfg.needs_reasoning_placeholder ~= nil then
			return prov_cfg.needs_reasoning_placeholder == true
		end
	end
	return fallback == true
end

function M.maybe_yield(i, stride)
	if i % (stride or 4096) == 0 and coroutine.isyieldable() then
		mare.task.yield()
	end
end

--- Memoize a zero-argument loader so a cold submodule or resource initializes once on first use.
--- Failed loads are not cached.
---@generic T
---@param loader fun(): T
---@return fun(): T
function M.lazy(loader)
	local loaded = false
	local value
	return function()
		if not loaded then
			value = loader()
			loaded = true
		end
		return value
	end
end

--- Bind a cold submodule's methods on `target` to lazy stubs while preserving outer wrappers
--- (such as `mare_lib.service` proxies or poison guards) installed afterward.
--- On first use, `loader()` runs under `pcall`; if it fails (or omits a declared method/field),
--- saved wrappers are restored and the error is re-raised (failed loads are not cached).
--- Returns `ensure, proxy`: `ensure()` initializes once and returns `loader()`'s result;
--- `proxy` forwards field reads to `ensure()[k]`.
function M.lazy_group(target, methods, loader, opts)
	local fast_unloaded = opts and opts.fast_unloaded or nil
	local extra_target = opts and opts.extra_target or nil
	local extra_fields = opts and opts.extra_fields or nil
	local loaded = false
	local real, api

	local function restore(saved, saved_extra)
		for _, m in ipairs(methods) do
			target[m] = saved[m]
		end
		if saved_extra then
			for _, f in ipairs(extra_fields) do
				extra_target[f] = saved_extra[f]
			end
		end
	end

	local function ensure()
		if not loaded then
			local saved = {}
			for _, m in ipairs(methods) do
				saved[m] = target[m]
			end
			local saved_extra
			if extra_target and extra_fields then
				saved_extra = {}
				for _, f in ipairs(extra_fields) do
					saved_extra[f] = extra_target[f]
				end
			end
			local ok, res = pcall(loader)
			if not ok then
				restore(saved, saved_extra)
				error(res, 0)
			end
			local captured = {}
			for _, m in ipairs(methods) do
				local fn = target[m]
				if fn == saved[m] or type(fn) ~= "function" then
					restore(saved, saved_extra)
					error("lazy_group loader did not define method `" .. tostring(m) .. "`", 0)
				end
				captured[m] = fn
				target[m] = saved[m]
			end
			if saved_extra then
				for _, f in ipairs(extra_fields) do
					if extra_target[f] == saved_extra[f] then
						restore(saved, saved_extra)
						error("lazy_group loader did not define field `" .. tostring(f) .. "`", 0)
					end
				end
			end
			real = captured
			api = res
			loaded = true
		end
		return api
	end

	for _, m in ipairs(methods) do
		local fast = fast_unloaded and fast_unloaded[m] or nil
		target[m] = function(...)
			if fast and not loaded then
				return fast(...)
			end
			ensure()
			return real[m](...)
		end
	end

	if extra_target and extra_fields then
		for _, f in ipairs(extra_fields) do
			extra_target[f] = function(...)
				ensure()
				return extra_target[f](...)
			end
		end
	end

	local proxy = setmetatable({}, {
		__index = function(_, k)
			return ensure()[k]
		end,
	})
	return ensure, proxy
end

--- Round a numeric timestamp into integer Unix epoch milliseconds (`unit == "s"` converts seconds).
---@param value any
---@param unit? "ms"|"s"
---@return integer|nil
function M.to_epoch_ms(value, unit)
	if type(value) ~= "number" or value ~= value or value == math.huge or value == -math.huge then
		return nil
	end
	if unit == "s" then
		return math.floor(value * 1000 + 0.5)
	end
	return math.floor(value + 0.5)
end

--- Return a `<close>`-able guard that invokes `cleanup()` at most once when its scope exits,
--- including on errors and `mare.task.select` coroutine aborts. Call `:disarm()` to cancel the
--- cleanup or `:release()` to run it early.
function M.guard(cleanup)
	assert(type(cleanup) == "function", "mare_lib.guard expects a cleanup function")
	local active = true
	local function run()
		if active then
			active = false
			cleanup()
		end
	end
	return setmetatable({
		disarm = function()
			active = false
		end,
		release = run,
	}, {
		__close = run,
	})
end

--- Coroutine-safe mutex backed by a 1-token `mare.task.channel()` whose lease is released via
--- `<close>` so `mare.task.select` aborts never lose the lock token.
function M.mutex()
	local tx, rx = mare.task.channel()
	tx:send(true)
	local m = {}
	function m:acquire()
		rx:recv()
		return M.guard(function()
			tx:send(true)
		end)
	end
	function m:with(fn, ...)
		local _lease <close> = self:acquire()
		return fn(...)
	end
	return m
end

return M
