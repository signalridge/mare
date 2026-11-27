-- Shared bounded paging helper for `baseline = true` SPI cuts (`session` entries and `agent` active_items).
local STRICT_JSON_NODE_BUDGET = 7500 -- leave space for the outer envelope when filling a page
local STRICT_JSON_MAX_NODES = 10000 -- strict_lua_to_json's actual node limit

local function count_json_nodes(v, seen, limit)
	if type(v) ~= "table" then
		return 1
	end
	if seen[v] then
		return limit + 1
	end
	seen[v] = true
	local n = 1
	for _, elem in pairs(v) do
		n = n + count_json_nodes(elem, seen, limit)
		if n > limit then
			seen[v] = nil
			return n
		end
	end
	seen[v] = nil
	return n
end

return function(list, count, cursor, max_bytes, limit)
	local start_idx = 1
	if cursor ~= nil then
		local n = tonumber(cursor)
		if not n or n < 1 or math.floor(n) ~= n then
			require("mare_lib").fail("invalid_arguments", "invalid baseline cursor")
		end
		start_idx = math.floor(n)
	end
	local max_items = nil
	if limit ~= nil then
		local n = tonumber(limit)
		if not n or n < 1 or math.floor(n) ~= n then
			require("mare_lib").fail("invalid_arguments", "limit must be an integer >= 1")
		end
		max_items = math.floor(n)
	end
	local cap = tonumber(max_bytes) or (512 * 1024)
	local node_cap = STRICT_JSON_NODE_BUDGET
	local out = {}
	local total_bytes = 2 -- "[]"
	local total_nodes = 2 -- outer result table + data array
	local i = start_idx
	while i <= count do
		if max_items and #out >= max_items then
			break
		end
		local item = list[i]
		local item_bytes = #mare.json.encode(item) + (#out > 0 and 1 or 0)
		local item_nodes = count_json_nodes(item, {}, STRICT_JSON_MAX_NODES)
		local over_bytes = (total_bytes + item_bytes + 64) > cap
		if #out == 0 then
			-- The soft page budget must not reject a single item that still fits strict JSON.
			local result_nodes = total_nodes + item_nodes + (i < count and 1 or 0) -- optional next_cursor
			if over_bytes or result_nodes > STRICT_JSON_MAX_NODES then
				require("mare_lib").fail(
					"baseline.item_too_large",
					"one baseline item exceeds the page limit; request a new baseline after reducing its size"
				)
			end
		elseif over_bytes or (total_nodes + item_nodes) > node_cap then
			break
		end
		out[#out + 1] = item
		total_bytes = total_bytes + item_bytes
		total_nodes = total_nodes + item_nodes
		i = i + 1
	end
	local next_cursor = (i <= count) and tostring(i) or nil
	return {
		data = mare.json.array(out),
		next_cursor = next_cursor,
	}
end
