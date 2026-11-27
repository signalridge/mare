-- permissions.argv: argv normalization (executable key, wrappers) and argv / cwd rule matching;
-- part of the `permissions` plugin (plugins/permissions.lua), loaded at the start of its apply.
return function(D)
	---------------------------------------------------------------------------
	-- rules
	---------------------------------------------------------------------------
	-- The execution directory is passed per call, never shared across concurrent votes.
	local function glob_ok(pattern, cwd, conservative)
		if not pattern then
			return true
		end
		local here = cwd or mare.cwd
		local globs = D.rules.path_globs(pattern)
		if not globs then
			return false
		end
		for _, glob in ipairs(globs) do
			local literal = glob:literal()
			if literal then
				if mare.fs.path_within(here, literal, conservative == true) then
					return true
				end
			elseif glob:matches(here, conservative == true) or glob:covers_recursive(here, conservative == true) then
				return true
			end
		end
		return false
	end
	-- Executable key: basename and platform case normalization.
	-- Only deny/prompt matching may use the core's unwrapped command; allow rules and session
	-- approvals keep path separators so a path-named program (`./git`) never borrows a bare-name
	-- allow rule (`git`).
	local FOLD = mare.os == "macos" or mare.os == "windows"
	local WIN = mare.os == "windows"
	local function is_win_path(w)
		return w:match("^[/\\]") or w:match("^%.%.?[/\\]") or w:match("^%a:[/\\]")
	end
	local function exe_norm(w, strip_dir, shell)
		w = tostring(w or "")
		if shell == "powershell" or (WIN and is_win_path(w)) then
			w = w:gsub("\\", "/")
		else
			w = w:gsub("\\", "")
		end
		if strip_dir then
			w = w:match("([^/]+)$") or w
		end
		if WIN then
			w = w:gsub("%.[eE][xX][eE]$", "")
		end
		return FOLD and w:lower() or w
	end
	local function exe_key(w, shell)
		return exe_norm(w, true, shell)
	end
	-- Allow rules and session approvals keep the original execution argv.
	local function norm_argv(argv, strip_dir, shell)
		local out = {}
		for i, word in ipairs(argv) do
			out[i] = i == 1 and exe_norm(word, strip_dir, shell) or word
		end
		return out
	end
	local function argv_match(rule, argv, unwrapped, cwd, shell)
		if type(rule.argv) ~= "table" or #rule.argv == 0 then
			return false
		end
		local strip_dir = rule.decision ~= "allow"
		argv = norm_argv(strip_dir and unwrapped or argv, strip_dir, shell)
		if #argv < #rule.argv then
			return false
		end
		for i, w in ipairs(rule.argv) do
			if argv[i] ~= (i == 1 and exe_norm(w, strip_dir, shell) or w) then
				return false
			end
		end
		for _, d in ipairs(rule.deny_args or {}) do
			for j = #rule.argv + 1, #argv do
				local a = argv[j]
				if a == d or string.sub(a, 1, #d + 1) == d .. "=" or (#d == 2 and string.sub(a, 1, 2) == d) then
					return false
				end
			end
		end
		return glob_ok(rule.cwd, cwd, rule.decision ~= "allow")
	end

	return { norm_argv = norm_argv, argv_match = argv_match, exe_key = exe_key, cwd_ok = glob_ok }
end
