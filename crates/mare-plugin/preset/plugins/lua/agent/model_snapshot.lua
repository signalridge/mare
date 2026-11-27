-- agent.model_snapshot: isolate model descriptors while retaining supported function-valued
-- parameter rules. This is distinct from JSON tool schemas, which reject functions.
local M = {}

function M.copy(value)
	local seen, budget = {}, 10000
	local function copy(v, depth)
		budget = budget - 1
		if budget < 0 or depth > 64 then
			error("model descriptor exceeds snapshot resource limit")
		end
		local kind = type(v)
		if kind ~= "table" then
			if kind == "nil" or kind == "boolean" or kind == "number" or kind == "string" or kind == "function" then
				return v
			end
			error("model descriptor contains unsupported " .. kind)
		end
		if seen[v] then
			return seen[v]
		end
		local result = {}
		seen[v] = result
		for key, item in next, v do
			if type(key) == "table" then
				error("model descriptor contains a table key")
			end
			result[key] = copy(item, depth + 1)
		end
		local mt = getmetatable(v)
		if mare.json.is_array(v) then
			mare.json.array(result)
		elseif mt ~= nil then
			if type(mt) ~= "table" then
				error("model descriptor has a protected metatable")
			end
			setmetatable(result, copy(mt, depth + 1))
		end
		return result
	end
	return copy(value, 0)
end

function M.equal(left, right)
	local seen_left, seen_right, budget = {}, {}, 10000
	local function equal(a, b, depth)
		budget = budget - 1
		if budget < 0 or depth > 64 or type(a) ~= type(b) then
			return false
		end
		if type(a) ~= "table" then
			return a == b
		end
		local array = mare.json.is_array(a)
		if array ~= mare.json.is_array(b) then
			return false
		end
		if seen_left[a] or seen_right[b] then
			return seen_left[a] == b and seen_right[b] == a
		end
		seen_left[a], seen_right[b] = b, a
		local n = 0
		for key, value in next, a do
			n = n + 1
			if not equal(value, b[key], depth + 1) then
				return false
			end
		end
		for _ in next, b do
			n = n - 1
		end
		return n == 0 and (array or equal(getmetatable(a), getmetatable(b), depth + 1))
	end
	return equal(left, right, 0)
end

return M
