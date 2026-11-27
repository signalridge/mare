-- Synchronous detached activity queries; no queue, hooks or persistence.
return function(D)
	local ctx, A = D.ctx, D.A
	---@type {tools: table[], system: string, source_message_count: integer, messages: table[], model: any, params: table}|nil
	local request = nil
	local streaming, running = {}, {}
	-- Baseline active items reflect only events already committed onto `ctx.bus` (`seq <= S`).
	local turn_item_seq = 0
	local emitted_msg = nil
	local emitted_tools = {}
	local function copy(value, seen)
		if type(value) ~= "table" then
			return value
		end
		seen = seen or {}
		if seen[value] then
			return seen[value]
		end
		local out = mare.json.is_array(value) and mare.json.array({}) or {}
		seen[value] = out
		for k, v in pairs(value) do
			out[k] = copy(v, seen)
		end
		return out
	end
	local function clear()
		request, streaming, running = nil, {}, {}
		turn_item_seq, emitted_msg, emitted_tools = 0, nil, {}
	end
	local function settle_run()
		streaming, running = {}, {}
		turn_item_seq, emitted_msg, emitted_tools = 0, nil, {}
	end
	ctx:on("turn/started", settle_run)
	ctx:on("turn/completed", settle_run)
	ctx:on("agent/messages-replaced", clear)
	local providers = {}
	local M = {}
	function M.register(caller, name, provider)
		mare.exec.check_mutation("activity provider registration")
		assert(
			type(name) == "string" and name:match("^[%w][%w_.:/-]*$") and #name <= 128,
			"invalid activity provider name"
		)
		assert(type(provider) == "function", "activity provider must be a function")
		if providers[name] then
			error("activity provider already registered: " .. name, 0)
		end
		local row = { provider = provider }
		providers[name] = row
		local function remove()
			if providers[name] == row then
				mare.exec.check_mutation("activity provider removal")
				providers[name] = nil
				ctx.bus:emit("agent/activity-changed", { namespace = name, kind = "removed" })
			end
		end
		caller:effect(function()
			return remove
		end, "agent activity " .. name)
		ctx.bus:emit("agent/activity-changed", { namespace = name, kind = "registered" })
		return remove
	end
	function M.request(req)
		request = copy({
			system = req.system or "",
			source_message_count = #A.messages,
			messages = mare.json.array(copy(req.messages)),
			tools = mare.json.array(copy(req.tools or {})),
			model = req.model and req.model.ref,
			params = ctx.llm:resolve_params(req.model, { request = req.params }).params,
		})
		for index, tool in ipairs(request.tools) do
			tool.parameters = mare.activity.capture(function()
				return req.tools[index].parameters
			end)
		end
		streaming = {}
	end
	function M.delta(ev)
		if ev.type == "text_delta" or ev.type == "reasoning_delta" then
			local field = ev.type == "text_delta" and "text" or "reasoning"
			local last = streaming[#streaming]
			if not last or last.type ~= field then
				last = { type = field, buffer = mare.stream.buffer() }
				streaming[#streaming + 1] = last
			end
			last.buffer:append(ev.delta or "")
		elseif ev.type == "tool_call_start" then
			streaming[#streaming + 1] = {
				type = "tool_call",
				id = ev.tool_call_id,
				name = ev.name,
				buffer = mare.stream.buffer(),
			}
		elseif ev.type == "tool_call_delta" then
			for _, b in ipairs(streaming) do
				if b.type == "tool_call" and b.id == ev.tool_call_id then
					b.buffer:append(ev.delta or "")
					break
				end
			end
		end
	end
	function M.settled()
		streaming = {}
	end
	ctx:on("step/started", function()
		turn_item_seq = 0
		emitted_msg = nil
	end)
	ctx:on("item/started", function(ev)
		local item = ev and ev.item
		if not item then
			return
		end
		if item.type == "assistant_message" then
			turn_item_seq = turn_item_seq + 1
			local base_id = A.turn_id or "turn"
			emitted_msg = {
				item_id = ev.item_id or (base_id .. ":msg:" .. tostring(turn_item_seq)),
				item = copy(item.message and { role = item.message.role or "assistant" } or item),
				content = {},
			}
		elseif item.type == "tool_call" then
			local cid = item.tool_call_id
			if cid then
				local call = { id = cid, name = item.name, arguments = item.arguments }
				running[cid] = { call = copy(call), output_tail = "" }
				emitted_tools[cid] = {
					item_id = ev.item_id or cid,
					item = {
						type = "tool_call",
						tool_call_id = cid,
						name = item.name,
						arguments = copy(item.arguments),
						info = copy(item.info),
					},
					content = {},
				}
			end
		end
	end)
	ctx:on("item/delta", function(ev)
		if not ev then
			return
		end
		if (ev.kind == "text" or ev.kind == "reasoning") and emitted_msg then
			local blocks = emitted_msg.content
			local last = blocks[#blocks]
			if not last or last.kind ~= ev.kind then
				last = { kind = ev.kind, buffer = mare.stream.buffer() }
				blocks[#blocks + 1] = last
			end
			last.buffer:append(ev.delta or "")
			return
		elseif ev.kind == "tool_arguments" and emitted_msg then
			local cid = ev.tool_call_id
			local found
			for _, b in ipairs(emitted_msg.content) do
				if b.kind == "tool_call" and b.id == cid then
					found = b
					break
				end
			end
			if not found and cid then
				found = { kind = "tool_call", id = cid, buffer = mare.stream.buffer() }
				emitted_msg.content[#emitted_msg.content + 1] = found
			end
			if found then
				found.buffer:append(ev.delta or "")
			end
			return
		end
		if ev.kind ~= "tool_output" then
			return
		end
		local cid = ev.tool_call_id or ev.item_id
		local row = cid and running[cid]
		if row then
			local text = ev.partial and (ev.partial.content or ev.partial.output) or ev.delta
			if type(text) == "table" then
				local parts = {}
				for _, b in ipairs(text) do
					if b.type == "text" then
						parts[#parts + 1] = b.text
					end
				end
				text = table.concat(parts, "\n")
			end
			if type(text) == "string" then
				local source = (ev.partial and (ev.partial.content or ev.partial.output)) and text
					or (row.output_tail .. text)
				local tail = mare.text.cut(source, 16384, { keep = "tail", unit = "bytes" })
				tail = mare.text.truncate(tail, { max_lines = 199, max_bytes = 1e9, keep = "tail" })
				row.output_tail = mare.text.cut(tail, 16384, { keep = "tail", unit = "bytes" })
				local tool_item = cid and emitted_tools[cid]
				if tool_item then
					tool_item.content = {
						{ kind = "tool_output", text = row.output_tail, offset = #row.output_tail },
					}
				end
			end
		end
	end)
	ctx:on("item/completed", function(ev)
		local item = ev and ev.item
		if not item then
			return
		end
		if item.type == "assistant_message" then
			emitted_msg = nil
		elseif item.type == "tool_call" then
			local cid = item.tool_call_id or ev.item_id
			if cid then
				running[cid] = nil
				emitted_tools[cid] = nil
			end
		end
	end)
	function M.baseline_items(include_text)
		local out = {}
		local function project_item(src)
			local blocks = mare.json.array()
			for i, b in ipairs(src.content) do
				local entry = {
					kind = b.kind,
					offset = b.buffer and b.buffer:len() or b.offset,
				}
				if include_text then
					entry.text = b.buffer and b.buffer:snapshot() or b.text
				end
				if b.id ~= nil then
					entry.id = b.id
				end
				if b.name ~= nil then
					entry.name = b.name
				end
				blocks[i] = entry
			end
			return {
				item_id = src.item_id,
				item = copy(src.item),
				content = blocks,
			}
		end
		if emitted_msg then
			out[#out + 1] = project_item(emitted_msg)
		end
		local tool_ids = {}
		for id in pairs(emitted_tools) do
			tool_ids[#tool_ids + 1] = id
		end
		table.sort(tool_ids)
		for _, id in ipairs(tool_ids) do
			out[#out + 1] = project_item(emitted_tools[id])
		end
		return out
	end

	function M.extras()
		local extra, extra_errors = {}, {}
		for name, row in pairs(providers) do
			local ok, value = pcall(mare.activity.capture, row.provider)
			if ok then
				extra[name] = value
			else
				extra_errors[name] = tostring(value)
			end
		end
		return extra, extra_errors
	end
	function M.snapshot()
		local tools = mare.json.array({})
		for _, row in pairs(running) do
			tools[#tools + 1] = copy(row)
		end
		table.sort(tools, function(a, b)
			return a.call.id < b.call.id
		end)
		local snapshot_streaming = mare.json.array({})
		for i, b in ipairs(streaming) do
			if b.type == "tool_call" then
				snapshot_streaming[i] = {
					type = "tool_call",
					id = b.id,
					name = b.name,
					raw_arguments = b.buffer:snapshot(),
				}
			else
				snapshot_streaming[i] = {
					type = b.type,
					text = b.buffer:snapshot(),
				}
			end
		end
		local extra, extra_errors = M.extras()
		local s = {
			version = 1,
			busy = A.busy,
			queued = #A.steer + #A.follow,
			turn_cancelled = A.token ~= nil and A.token:is_cancelled() or false,
			session_id = A.session_id,
			turn_id = A.busy and A.turn_id or nil,
			messages = mare.json.array(copy(A.messages)),
			request = copy(request),
			streaming = snapshot_streaming,
			running_tools = tools,

			extra = extra,
			extra_errors = extra_errors,
		}
		if s.request then
			for index, tool in ipairs(s.request.tools) do
				tool.parameters = mare.activity.capture(function()
					return request.tools[index].parameters
				end)
			end
		end
		return s
	end
	return M
end
