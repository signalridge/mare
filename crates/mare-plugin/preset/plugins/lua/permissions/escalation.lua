-- permissions.escalation: escalation requests as per-dimension increments over the preset's
-- sandbox; part of the `permissions` plugin (plugins/permissions.lua), loaded at the start of its apply.
return function(D)
	local copy = D.copy
	local with_common, base_spec = D.presets.with_common, D.presets.base_spec

	---------------------------------------------------------------------------
	-- escalation requests
	---------------------------------------------------------------------------
	---------------------------------------------------------------------------
	-- per-dimension increments:
	-- inc = { all = true } | { network = true | { host, ... }, write = { abs dir, ... } }
	-- An escalation adds only what it names to the base policy; the network opens only when
	-- an increment says so explicitly.
	---------------------------------------------------------------------------
	local S = mare.proc.sandbox
	local function inc_empty(inc)
		if not inc then
			return true
		end
		local network = inc.network == true or (type(inc.network) == "table" and #inc.network > 0)
		return not inc.all and not network and not (inc.write and #inc.write > 0) and not (inc.read and #inc.read > 0)
	end
	local function hosts_cover(hosts, h)
		if type(hosts) ~= "table" or #hosts == 0 then
			return false
		end
		return S.check("network", h, { level = "read-only", network = { allow = hosts, allow_local = true } }) == true
	end
	--- one approved store per kind accumulates as a union
	local function inc_union(a, b)
		if inc_empty(a) then
			return b and copy(b) or {}
		end
		if inc_empty(b) then
			return copy(a)
		end
		if a.all or b.all then
			return { all = true }
		end
		local out = { write = {}, read = {} }
		if a.network == true or b.network == true then
			out.network = true
		elseif a.network or b.network then
			local net = {}
			for _, x in ipairs({ a.network or {}, b.network or {} }) do
				for _, h in ipairs(x) do
					table.insert(net, h)
				end
			end
			out.network = net
		end
		for _, x in ipairs({ a.write or {}, b.write or {} }) do
			for _, w in ipairs(x) do
				table.insert(out.write, w)
			end
		end
		for _, x in ipairs({ a.read or {}, b.read or {} }) do
			for _, r in ipairs(x) do
				table.insert(out.read, r)
			end
		end
		return out
	end
	local function protected_meta_segment(p)
		if type(p) ~= "string" then
			return false
		end
		local norm = mare.os == "windows" and p:gsub("\\", "/"):lower() or (mare.os == "macos" and p:lower() or p)
		return norm:match("/%.git$") ~= nil
			or norm:match("/%.git/") ~= nil
			or norm:match("/%.mare$") ~= nil
			or norm:match("/%.mare/") ~= nil
	end
	local function write_covered(w, roots)
		if not protected_meta_segment(w) then
			return S.within(w, roots or {}, { stable_roots = true })
		end
		-- OS sandboxes keep `.git` / `.mare` read-only inside granted ancestor directories;
		-- only a stored grant that itself sits inside the protected subtree covers `w`.
		local meta_roots = {}
		for _, r in ipairs(roots or {}) do
			if protected_meta_segment(r) then
				meta_roots[#meta_roots + 1] = r
			end
		end
		return #meta_roots > 0 and S.within(w, meta_roots, { stable_roots = true }) or false
	end
	--- the part of `req` not already inside `store` (nil when fully covered)
	local function inc_remainder(req, store)
		store = store or {}
		if req.all then
			return not store.all and { all = true } or nil
		end
		if store.all then
			return nil
		end
		local out = { write = {}, read = {} }
		if req.network == true and store.network ~= true then
			out.network = true
		elseif type(req.network) == "table" and store.network ~= true then
			local miss = {}
			for _, h in ipairs(req.network) do
				if not hosts_cover(store.network, h) then
					miss[#miss + 1] = h
				end
			end
			if #miss > 0 then
				out.network = miss
			end
		end
		for _, w in ipairs(req.write or {}) do
			if not write_covered(w, store.write) then
				table.insert(out.write, w)
			end
		end
		for _, r in ipairs(req.read or {}) do
			if not S.within(r, store.read or {}, { stable_roots = true }) then
				table.insert(out.read, r)
			end
		end
		return not inc_empty(out) and out or nil
	end
	local function workspace_root()
		local ok, phys = pcall(mare.fs.realpath, mare.cwd)
		return (ok and type(phys) == "string" and phys ~= "") and phys or mare.cwd
	end
	--- an approval narrows the request, never widens it: network only
	--- if both allow it, paths only inside the request; anything unapproved is dropped
	local function inc_narrow(req, appr)
		if type(appr) ~= "table" then
			return req
		end
		if req.all then
			return appr.all and req or inc_narrow({ network = true, write = { workspace_root() } }, appr)
		end
		local out = { write = {}, read = {} }
		if appr.all then
			return copy(req)
		end
		if req.network == true then
			out.network = appr.network
		elseif type(req.network) == "table" then
			if appr.network == true then
				out.network = req.network
			elseif type(appr.network) == "table" then
				local keep = {}
				for _, h in ipairs(req.network) do
					if hosts_cover(appr.network, h) then
						keep[#keep + 1] = h
					end
				end
				for _, h in ipairs(appr.network) do
					if hosts_cover(req.network, h) then
						keep[#keep + 1] = h
					end
				end
				out.network = #keep > 0 and keep or nil
			end
		end
		for _, kind in ipairs({ "read", "write" }) do
			for _, path in ipairs(req[kind] or {}) do
				local covered = kind == "write" and write_covered(path, appr.write)
					or kind == "read" and S.within(path, appr.read or {}, { stable_roots = true })
				if covered then
					table.insert(out[kind], path)
				end
				for _, allowed in ipairs(appr[kind] or {}) do
					local rev = kind == "write" and write_covered(allowed, { path })
						or kind == "read" and S.within(allowed, { path }, { stable_roots = true })
					if rev then
						table.insert(out[kind], allowed)
					end
				end
			end
		end
		return out
	end
	--- does the base policy already allow everything `inc` asks for?
	local function base_covers(base, inc)
		if base == false then
			return true
		end
		if inc.all then
			return false
		end
		if inc.network == true and base.network ~= true then
			return false
		end
		if type(inc.network) == "table" and base.network ~= true then
			for _, h in ipairs(inc.network) do
				if S.check("network", h, base) ~= true then
					return false
				end
			end
		end
		for _, w in ipairs(inc.write or {}) do
			if S.check("write", w .. "/.mare-probe", base) ~= true then
				return false
			end
		end
		for _, r in ipairs(inc.read or {}) do
			if S.check("read", r, base) ~= true then
				return false
			end
		end
		return true
	end
	--- base + increment → the call's spec ("danger-full-access" spelled out for `all`)
	local function apply_inc(base, inc)
		if inc.all then
			return "danger-full-access"
		end
		if base == false then
			return nil
		end
		local s = copy(base)
		if s.network == nil then
			s.network = false -- spelled out: a missing network is restricted
		end
		if inc.write and #inc.write > 0 then
			if s.level == "read-only" then
				-- only the requested paths become writable (not the workspace)
				s.level = "workspace-write"
				s.exclude_tmpdir_env_var, s.exclude_slash_tmp = true, true
				s.writable = nil
				s.write = copy(inc.write)
			else
				s.writable = s.writable or {}
				for _, w in ipairs(inc.write) do
					table.insert(s.writable, w)
				end
			end
		end
		if inc.read and #inc.read > 0 then
			s.readable = s.readable or {}
			for _, r in ipairs(inc.read) do
				table.insert(s.readable, r)
			end
		end
		if inc.network == true then
			s.network = true
		elseif type(inc.network) == "table" and s.network ~= true then
			if type(s.network) == "table" then
				s.network.allow = s.network.allow or {}
				for _, h in ipairs(inc.network) do
					table.insert(s.network.allow, h)
				end
			else
				s.network = { default = "deny", allow = copy(inc.network) }
			end
		end
		return with_common(s)
	end
	-- a path separator, and a path's parent directory: Windows paths resolve with `\` (and
	-- may mix in `/`); elsewhere `\` is an ordinary file name character
	local SEP = mare.os == "windows" and "[/\\]" or "/"
	local PARENT = mare.os == "windows" and "^(.*)[/\\][^/\\]*$" or "^(.*)/[^/]*$"
	local function is_abs_path(p)
		return p:match("^/") or (mare.os == "windows" and p:match("^%a:[/\\]"))
	end
	local function join_rel(base, rel)
		if is_abs_path(rel) then
			return rel
		end
		return base .. "/" .. rel
	end
	local function same_physical(a, b)
		if mare.os == "windows" then
			return a:gsub("\\", "/"):lower() == b:gsub("\\", "/"):lower()
		end
		return a == b
	end
	local function no_symlink_dir(p)
		local ok, rp = pcall(mare.fs.resolve_path, p)
		if not ok or not rp or not same_physical(rp.logical, rp.physical) then
			return nil
		end
		local st = mare.fs.stat(rp.physical)
		if not st or st.kind ~= "dir" then
			return nil
		end
		return rp.physical
	end
	local function valid_git_head(dir)
		local head = mare.fs.read(dir .. "/HEAD", { no_follow = true, max_bytes = 256 })
		if type(head) ~= "string" then
			return false
		end
		local trimmed = head:match("^%s*(.-)%s*$")
		return trimmed:match("^ref:%s*refs/") ~= nil
			or ((#trimmed == 40 or #trimmed == 64) and trimmed:match("^%x+$") ~= nil)
	end
	local function valid_git_dir_layout(dir)
		return no_symlink_dir(dir .. "/objects") ~= nil
			and no_symlink_dir(dir .. "/refs") ~= nil
			and valid_git_head(dir)
	end
	local function is_dot_git_path(abs)
		if mare.os == "windows" or mare.os == "macos" then
			return abs:lower():match(SEP .. "%.git$") ~= nil
		end
		return abs:match(SEP .. "%.git$") ~= nil
	end
	local function resolve_in_cwd(path, cwd)
		if cwd and D.lib.is_project_relative(path) then
			path = cwd .. "/" .. path
		end
		return mare.fs.resolve(path)
	end
	--- Resolve `.git` pointer file and optional linked-worktree `commondir` strictly without symlinks.
	local function resolve_git_pointer_writes(abs)
		-- Canonicalize only the containing directory first so worktrees under symlinked
		-- ancestors (e.g. macOS `/var` -> `/private/var` or symlinked repo roots) work while
		-- `mare.fs.read(..., { no_follow = true })` still strictly refuses a symlinked `.git`.
		local parent = abs:match(PARENT) or "."
		local ok_parent, parent_phys = pcall(mare.fs.realpath, parent)
		if not ok_parent or type(parent_phys) ~= "string" or parent_phys == "" then
			return nil, "write scope: invalid .git parent directory"
		end
		local abs_phys = parent_phys .. "/.git"
		local ptr = mare.fs.read(abs_phys, { no_follow = true, max_bytes = 65536 })
		local gd = ptr and ptr:match("^%s*gitdir:%s*(.-)%s*$")
		if not gd or gd == "" or gd:find("[%z\r\n]") then
			return nil, "write scope: invalid .git pointer file"
		end
		local gd_cand = join_rel(parent_phys, gd)
		local gd_phys = no_symlink_dir(gd_cand)
		if not gd_phys then
			return nil, "write scope: symlinked or invalid .git gitdir target"
		end
		local cd_raw, _, cd_kind = mare.fs.read(gd_phys .. "/commondir", { no_follow = true, max_bytes = 65536 })
		if not cd_raw then
			if cd_kind == "not_found" then
				if not valid_git_dir_layout(gd_phys) then
					return nil, "write scope: .git gitdir is not a valid git directory"
				end
				return { write = { gd_phys } }
			end
			return nil, "write scope: invalid git commondir"
		end
		local cd_rel = cd_raw:match("^%s*(.-)%s*$")
		if cd_rel == "" or cd_rel:find("[%z\r\n]") then
			return nil, "write scope: invalid git commondir"
		end
		local cd_phys = no_symlink_dir(join_rel(gd_phys, cd_rel))
		if not cd_phys then
			return nil, "write scope: symlinked or missing git commondir"
		end
		local worktrees_dir = no_symlink_dir(cd_phys .. "/worktrees")
		if
			not worktrees_dir
			or not S.within(gd_phys, { worktrees_dir }, { stable_roots = true })
			or same_physical(gd_phys, worktrees_dir)
		then
			return nil, "write scope: gitdir is not a worktree inside commondir"
		end
		if
			not no_symlink_dir(cd_phys .. "/objects")
			or not no_symlink_dir(cd_phys .. "/refs")
			or not valid_git_head(cd_phys)
			or not valid_git_head(gd_phys)
		then
			return nil, "write scope: commondir is not a valid git repository"
		end
		local back = mare.fs.read(gd_phys .. "/gitdir", { no_follow = true, max_bytes = 65536 })
		local back_rel = back and back:match("^%s*(.-)%s*$")
		if not back_rel or back_rel == "" or back_rel:find("[%z\r\n]") then
			return nil, "write scope: invalid worktree gitdir backlink"
		end
		local ok_abs, abs_rp = pcall(mare.fs.resolve_path, abs_phys)
		local ok_back, back_rp = pcall(mare.fs.resolve_path, join_rel(gd_phys, back_rel))
		if
			not ok_abs
			or not ok_back
			or not same_physical(abs_rp.logical, abs_rp.physical)
			or not same_physical(back_rp.logical, back_rp.physical)
			or not same_physical(back_rp.physical, abs_rp.physical)
		then
			return nil, "write scope: worktree gitdir backlink does not match .git"
		end
		return { write = { gd_phys, cd_phys } }
	end
	--- `sandbox_permissions` → increment
	local function scope_inc(scope, cwd)
		if scope == "all" or scope == "danger-full-access" then
			return { all = true }
		elseif scope == "network" then
			return { network = true }
		elseif scope == "workspace-write" then
			return { write = { workspace_root() } }
		end
		local host = scope:match("^network:(.+)$")
		if host then
			return { network = { host } }
		end
		local path = scope:match("^write:(.+)$")
		if path then
			local abs = resolve_in_cwd(path, cwd)
			local st = mare.fs.stat(abs)
			-- a worktree/submodule `.git` is a pointer file: `write:.git` grants the gitdir it
			-- names (and linked-worktree `commondir` when valid), not the workspace around it
			if st and st.kind ~= "dir" and is_dot_git_path(abs) then
				return resolve_git_pointer_writes(abs)
			end
			-- a file (existing, or one the command is about to create) is checked against its
			-- parent directory: the grant is that directory
			if not st or st.kind ~= "dir" then
				abs = abs:match(PARENT) or abs
				st = mare.fs.stat(abs)
				if st and st.kind ~= "dir" then
					st = nil
				end
			end
			if not st then
				return nil, "write scope: neither " .. path .. " nor its parent directory exists"
			end
			local ok_rp, phys = pcall(mare.fs.realpath, abs)
			if not ok_rp or type(phys) ~= "string" or phys == "" then
				return nil, "write scope: cannot resolve physical directory for " .. path
			end
			local phys_st = mare.fs.stat(phys)
			if not phys_st or phys_st.kind ~= "dir" then
				return nil, "write scope: " .. path .. " does not resolve to an existing directory"
			end
			return { write = { phys } }
		end
		return nil,
			"invalid sandbox_permissions: "
				.. tostring(scope)
				.. ' (use "network", "network:<host>", "write:<path>" or "all")'
	end
	--- human text for an increment (dialogs)
	local function inc_text(inc)
		if inc.all then
			return "all (no sandbox)"
		end
		local parts = {}
		if inc.network == true then
			parts[#parts + 1] = "network"
		elseif type(inc.network) == "table" then
			parts[#parts + 1] = "network:" .. table.concat(inc.network, ",")
		end
		for _, r in ipairs(inc.read or {}) do
			parts[#parts + 1] = "read:" .. r
		end
		for _, w in ipairs(inc.write or {}) do
			parts[#parts + 1] = "write:" .. w
		end
		return table.concat(parts, " ")
	end
	--- the base policy with only file writes widened to the workspace (an approved write
	--- under read-only): network, reads and sockets stay as they were
	local function writes_widened(base)
		return apply_inc(base, { write = { workspace_root() } })
	end
	--- widen the current preset's sandbox by one requested scope.
	--- -> spec | nil, err ; wider = whether approval is needed ; inc = the increment
	local function widen(scope, cwd)
		local base = base_spec()
		if base == false then
			return nil, nil, false -- already unrestricted: no per-call override
		end
		local inc, err = scope_inc(scope, cwd)
		if not inc then
			return nil, err
		end
		if base_covers(base, inc) then
			return base, nil, false, inc
		end
		return apply_inc(base, inc), nil, true, inc
	end

	return {
		inc_empty = inc_empty,
		apply_inc = apply_inc,
		base_covers = base_covers,
		inc_union = inc_union,
		inc_remainder = inc_remainder,
		inc_narrow = inc_narrow,
		inc_text = inc_text,
		writes_widened = writes_widened,
		workspace_root = workspace_root,
		is_dot_git_path = is_dot_git_path,
		write_covered = write_covered,
		resolve_git_pointer_writes = resolve_git_pointer_writes,
		widen = widen,
	}
end
