-- ui.explore_summary: the header of a group of consecutive read / list / search calls as
-- counts, "Read 3 files, searched 2 patterns, listed 1 directory". Collapsed
-- read/search group: present tense while it runs and past tense when done (CHANGELOG.md:4868),
-- "Listed N directories" for listings (:3714)'s "Explored" header carries no counts.
-- Pure: the ui plugin hands it the group's items ({ kind = "Read" | "ListFiles" | "Search",
-- label = … }).
local M = {}

local PHRASES = {
	-- kind, past verb, present verb, singular, plural
	{ "Read", "read", "reading", "file", "files" },
	{ "Search", "searched", "searching", "pattern", "patterns" },
	{ "ListFiles", "listed", "listing", "directory", "directories" },
}

--- counts per kind: reads count distinct files (the same file read twice is one file), searches
--- and listings count calls
function M.counts(items)
	local n, seen = { Read = 0, Search = 0, ListFiles = 0 }, {}
	for _, it in ipairs(items) do
		if it.kind == "Read" then
			local key = tostring(it.label or (it.call and it.call.name) or "")
			if not seen[key] then
				seen[key] = true
				n.Read = n.Read + 1
			end
		elseif n[it.kind] then
			n[it.kind] = n[it.kind] + 1
		end
	end
	return n
end

--- "Read 3 files, searched 2 patterns" (`running`: "Reading 3 files, searching 2 patterns")
function M.text(items, running)
	local n, parts = M.counts(items), {}
	for _, p in ipairs(PHRASES) do
		local count = n[p[1]]
		if count > 0 then
			local verb = running and p[3] or p[2]
			parts[#parts + 1] = ("%s %d %s"):format(verb, count, count == 1 and p[4] or p[5])
		end
	end
	if #parts == 0 then
		return running and "Exploring" or "Explored"
	end
	local text = table.concat(parts, ", ")
	return (text:gsub("^%l", string.upper))
end

return M
