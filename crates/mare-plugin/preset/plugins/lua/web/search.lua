-- Scoped concurrent queries: collector wins only after all successful results settle.
return function(run, request, call, limit)
	local queries, seen = {}, {}
	for _, q in ipairs(request.queries) do
		if not seen[q] then
			queries[#queries + 1] = q
			seen[q] = true
		end
	end
	local tx, rx = mare.task.channel()
	local branches = {}
	for index, query in ipairs(queries) do
		branches[#branches + 1] = function()
			local input = { queries = { query }, mode = request.mode, options = request.options }
			local ok, value = pcall(run, input, call)
			tx:send({ index = index, ok = ok, value = value })
			call.cancel:wait()
		end
	end
	branches[#branches + 1] = function()
		local results = {}
		for _ = 1, #queries do
			local result = assert(rx:recv(), "WEB_QUERY_CHANNEL_CLOSED")
			assert(result.ok, result.value)
			local items = type(result.value) == "table" and result.value.data or nil
			assert(type(items) == "table", "WEB_INVALID_RESULT")
			results[result.index] = {
				data = items,
				content = result.value.content,
				truncated = result.value.truncated,
			}
		end
		local data, urls, answers = mare.json.array({}), {}, {}
		local rank, truncated = 1, false
		for i, result in ipairs(results) do
			if result.content and result.content ~= "" then
				answers[#answers + 1] = #queries > 1 and (queries[i] .. "\n" .. result.content) or result.content
			end
			truncated = truncated or result.truncated
		end
		while true do
			local found = false
			for _, result in ipairs(results) do
				local source = result.data[rank]
				if source then
					found = true
					assert(type(source.url) == "string", "WEB_INVALID_RESULT: source URL required")
					if not urls[source.url] then
						if #data >= limit then
							truncated = true
						else
							urls[source.url] = true
							data[#data + 1] = source
						end
					end
				end
			end
			if not found then
				break
			end
			rank = rank + 1
		end
		return { data = data, content = table.concat(answers, "\n\n"), truncated = truncated }
	end
	local winner, result = mare.task.select(table.unpack(branches))
	assert(winner == #branches, "WEB_ABORTED")
	return result
end
