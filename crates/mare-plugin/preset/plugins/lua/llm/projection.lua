-- Content projection immediately before the provider boundary. Auth/model routing is untouched.
local M = {}
function M.request(req)
	local copy = table.copy(req)
	for _, key in ipairs({ "system", "messages", "tools", "params", "params_overlay", "native" }) do
		if req[key] ~= nil then
			if key == "messages" and type(req.messages) == "table" then
				local cleaned = {}
				for i, m in ipairs(req.messages) do
					if type(m) == "table" and m.turn_id ~= nil then
						local mc = table.copy(m)
						mc.turn_id = nil
						cleaned[i] = mc
					else
						cleaned[i] = m
					end
				end
				copy.messages = (mare.projection.project(cleaned, { lossy = true }))
			else
				copy[key] = (mare.projection.project(req[key], { lossy = true }))
			end
		end
	end
	return copy
end
function M.call(fn, ...)
	local result = table.pack(pcall(fn, ...))
	if not result[1] then
		local err = type(result[2]) == "table" and result[2] or tostring(result[2])
		error(mare.projection.project(err), 0)
	end
	for i = 2, result.n do
		result[i] = mare.projection.project(result[i])
	end
	return table.unpack(result, 2, result.n)
end
-- One independent native buffer for each decoded text channel/id. JSON tool argument
-- fragments are withheld while registered values exist; the final structured call is projected.
function M.sink()
	local revision, streams, order = mare.projection.revision(), {}, {}
	local active = mare.projection.active()
	local filter = {}
	function filter:push(ev)
		if mare.projection.revision() ~= revision then
			error("PROJECTION_CHANGED: registrations changed during streaming", 0)
		end
		if not active then
			return ev
		end
		if ev.type == "tool_call_delta" then
			return nil
		end
		if ev.type ~= "text_delta" and ev.type ~= "reasoning_delta" then
			return mare.projection.project(ev)
		end
		assert(type(ev.delta) == "string", "stream delta must be UTF-8 text")
		local key = ev.type
			.. "\0"
			.. type(ev.id)
			.. ":"
			.. tostring(ev.id)
			.. "\0"
			.. type(ev.tool_call_id)
			.. ":"
			.. tostring(ev.tool_call_id)
		local entry = streams[key]
		if not entry then
			local metadata = table.copy(ev)
			metadata.delta = nil
			entry = { stream = mare.projection.stream(), event = mare.projection.project(metadata) }
			streams[key] = entry
			order[#order + 1] = entry
		end
		local text = entry.stream:push(ev.delta)
		if text == "" then
			return nil
		end
		local safe = table.copy(entry.event)
		safe.delta = text
		return safe
	end
	function filter:finish()
		if mare.projection.revision() ~= revision then
			error("PROJECTION_CHANGED: registrations changed during streaming", 0)
		end
		local out = {}
		for _, entry in ipairs(order) do
			local text = entry.stream:finish()
			if text ~= "" then
				local ev = table.copy(entry.event)
				ev.delta = text
				out[#out + 1] = ev
			end
		end
		return out
	end
	function filter:close()
		for _, entry in ipairs(order) do
			entry.stream:close()
		end
	end
	return filter
end
return M
