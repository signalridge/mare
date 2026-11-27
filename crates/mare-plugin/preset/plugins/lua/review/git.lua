-- review.git: git command runner and diff/branch helpers;
-- part of the `review` plugin (plugins/review.lua).
local M = {}

-- diffs in the one format split_files reads, whatever the user's git config says
-- (an external diff tool, mnemonic or no prefixes, color)
local DIFF_OPTS = { "--no-color", "--no-ext-diff", "--src-prefix=a/", "--dst-prefix=b/" }

function M.run(args, ok_codes)
	if args[1] == "diff" or args[1] == "show" then
		local a2 = { args[1] }
		for _, o in ipairs(DIFF_OPTS) do
			a2[#a2 + 1] = o
		end
		for i = 2, #args do
			if args[i] ~= "--no-color" then
				a2[#a2 + 1] = args[i]
			end
		end
		args = a2
	end
	local r = mare.proc.run({ cmd = "git", args = args, max_output = 8 * 1024 * 1024, timeout_ms = 10000 })
	if r.code ~= 0 and not (ok_codes and ok_codes[r.code]) then
		return nil, (r.stderr ~= "" and r.stderr or r.stdout)
	end
	return r.stdout
end

--- split `git diff` output per file: { {path, text, adds, dels} }
function M.split_files(diff)
	local files, cur = {}, nil
	for line in string.gmatch(diff .. "\n", "(.-)\n") do
		local p = string.match(line, "^diff %-%-git a/.- b/(.+)$")
		if p then
			cur = { path = p, lines = { line }, adds = 0, dels = 0 }
			files[#files + 1] = cur
		elseif cur then
			cur.lines[#cur.lines + 1] = line
			if string.find(line, "^%+") and not string.find(line, "^%+%+%+") then
				cur.adds = cur.adds + 1
			elseif string.find(line, "^%-") and not string.find(line, "^%-%-%-") then
				cur.dels = cur.dels + 1
			end
		end
	end
	for _, f in ipairs(files) do
		f.text = table.concat(f.lines, "\n")
		f.lines = nil
	end
	return files
end

function M.short(sha)
	return string.sub(sha, 1, 7)
end

function M.branches()
	local out = {}
	for l in string.gmatch(M.run({ "branch", "--format=%(refname:short)" }) or "", "[^\n]+") do
		out[#out + 1] = l
	end
	return out
end

return M
