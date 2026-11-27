-- ui.classify: exploration-call classification (Read / ListFiles / Search / Unknown);
-- part of the `ui` plugin (plugins/ui.lua), loaded at the start of its apply.
-- Exploration grouping:
-- each call is classified Read / ListFiles / Search / Unknown; consecutive non-Unknown calls
-- form one group whose header counts them ("Reading 3 files, searching 2 patterns" while
-- running, "Read 3 files, searched 2 patterns" when done). Ctrl+T keeps every call.
local function first_arg(words, from)
	for i = from or 2, #words do
		local w = words[i]
		if not string.find(w, "^%-") then
			return w, i
		end
	end
	return nil
end
local function classify(call)
	local a = call.arguments or {}
	local n = call.name
	if n == "read" then
		return "Read", a.path
	elseif n == "ls" then
		return "ListFiles", a.path or "."
	elseif n == "grep" or n == "search" or n == "rg" then
		return "Search", a.pattern or a.query
	elseif n == "find" or n == "glob" or n == "fd" then
		return "ListFiles", a.pattern or a.path
	elseif n ~= "bash" then
		return "Unknown"
	end
	local cmd = a.command or ""
	-- ignore a leading `cd dir &&` and anything after a pipe into head/tail/wc
	cmd = string.gsub(cmd, "^%s*cd%s+[^;&|]+%s*&&%s*", "")
	cmd = string.gsub(cmd, "%s*|%s*(head|tail|wc)[^|]*$", "")
	if string.find(cmd, "[;&|><`$]") then
		return "Unknown"
	end
	local words = {}
	for w in string.gmatch(cmd, "%S+") do
		words[#words + 1] = (string.gsub(w, "^['\"](.*)['\"]$", "%1"))
	end
	local p = words[1]
	if not p then
		return "Unknown"
	end
	if p == "cat" or p == "head" or p == "tail" or p == "less" or p == "bat" or p == "nl" then
		return "Read", words[#words]
	elseif p == "sed" and table.find(words, "-n") then
		return "Read", words[#words]
	elseif p == "ls" or p == "tree" or p == "eza" or p == "exa" or p == "fd" or p == "find" then
		return "ListFiles", first_arg(words) or "."
	elseif p == "rg" and table.find(words, "--files") then
		return "ListFiles", first_arg(words) or "."
	elseif p == "rg" or p == "grep" or p == "ag" or p == "ack" then
		local q, i = first_arg(words)
		return "Search", q and (q .. ((words[(i or 0) + 1] and (" in " .. words[i + 1])) or "")) or nil
	elseif p == "git" and words[2] == "grep" then
		return "Search", first_arg(words, 3)
	end
	return "Unknown"
end

return { classify = classify }
