-- Snapshot a raw Lua table graph without invoking caller-controlled metamethods.
local function snapshot(root)
	local graph = {}
	local function visit(value)
		if type(value) ~= "table" or graph[value] then
			return
		end
		local saved = { values = {}, count = 0, metatable = getmetatable(value) }
		graph[value] = saved
		visit(saved.metatable)
		for k, v in next, value do
			saved.count = saved.count + 1
			saved.values[k] = v
			visit(k)
			visit(v)
		end
	end
	visit(root)
	return graph
end
local function unchanged(graph)
	for original, saved in next, graph do
		if not rawequal(getmetatable(original), saved.metatable) then
			return false
		end
		local count = 0
		for k, v in next, original do
			count = count + 1
			if not rawequal(rawget(saved.values, k), v) then
				return false
			end
		end
		if count ~= saved.count then
			return false
		end
	end
	return true
end
return { snapshot = snapshot, unchanged = unchanged }
