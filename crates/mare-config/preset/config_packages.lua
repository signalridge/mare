--- mare.plugins.add { "owner/repo", { "owner/repo", only, version|tag|branch|commit, opts, opts_extend,
---   config, init, enabled, optional, dependencies, submodules, name }, "owner/repo:subdir",
---   { dir = "~/code/x" }, { import = "packages" } }
--- (lazy.nvim style spec): normalized here; versions, installing and the lock are Rust's.
--- Every declaration is kept as a fragment; `core.config.state.finalize_packages()` (after the config files
--- ran) merges the fragments of each package (lazy.nvim `core/plugin.lua` / `meta.lua`):
--- scalars: the last declaration wins; `dependencies` and `only` are merged; `opts` deep-merge
--- with the last write winning (lazy `Util.merge`), `opts(pkg, opts)` functions transform the
--- result, `opts_extend` paths append lists instead of replacing them; any `enabled = false`
--- disables the package (it keeps its lock entry), and so are the dependencies only it needs;
--- a package declared only with `optional = true` is dropped. Package opts are
--- `{ <plugin> = {...} }` defaults under that plugin's options (mare.plugin wins).

local mare, C, core = ...
local state = core.config.state
mare.plugins = mare.plugins or {}
local warn = state.warn
local merge = state.merge
local deepcopy = state.deepcopy
local key_get = state.key_get
local key_set = state.key_set
local guard_option_write = state.guard_option_write

local function split_dots(s)
	local out = {}
	for seg in string.gmatch(s, "[^%.]+") do
		out[#out + 1] = seg
	end
	return out
end

--- a full git address by git's rules (mare-plugin-manager `source::classify`): `<helper>::`, `scheme://`,
--- an absolute or drive path, or scp `[user@]host:path` (a colon before the first `/`, not a
--- drive letter); anything else is a short name, where `owner/repo:subdir` is Yazi's subdir
local function is_address(s)
	if s:find("^%a[%w+.-]*::") or s:find("^%a[%w+.-]*://") then
		return true
	end
	if s:find("^[/\\]") or s:find("^%a:[/\\]") or s:find("^%a:$") then
		return true
	end
	local colon, slash = s:find(":", 1, true), s:find("/", 1, true)
	return colon ~= nil and colon > 1 and (slash == nil or colon < slash)
end

local function split_subdir(name)
	if is_address(name) then
		return name
	end
	local repo, sub = name:match("^([^:]+):(.+)$")
	if repo then
		return repo, sub
	end
	return name
end

-- per evaluation (this chunk runs again for every evaluation of the config)
local P = {
	order = {}, -- package ids in first-declaration order
	frags = {}, -- id -> { {fields, deps}, ... }
	explicit = {}, -- id -> declared directly (not only as somebody's dependency)
	needed = {}, -- id -> { [needer id] = true }
	imported = {}, -- import module -> true (lazy.nvim `self.modules`: each imported once)
	stack = {}, -- imports being evaluated (cycle detection)
}
local MAX_IMPORT_DEPTH = 16
local MAX_DECL_DEPTH = 32

local function find_pkg(name)
	for _, e in ipairs(C.packages or {}) do
		if e.name == name then
			return e
		end
	end
end

local add_decl

--- lazy.nvim `Util.lsmod`: `lua/<mod>.lua`, `lua/<mod>/init.lua`, `lua/<mod>/*.lua` and
--- `lua/<mod>/<dir>/init.lua`, sorted
local function module_files(mod)
	local base = mare.paths.config .. "/lua/" .. mod:gsub("%.", "/")
	local out = {}
	if mare.fs.exists(base .. ".lua") then
		out[#out + 1] = { mod, base .. ".lua" }
	end
	local entries = {}
	for _, f in ipairs(mare.fs.list(base) or {}) do
		local n = type(f) == "table" and f.name or f
		if type(n) == "string" then
			entries[#entries + 1] = n
		end
	end
	table.sort(entries)
	for _, n in ipairs(entries) do
		if n == "init.lua" then
			table.insert(out, 1, { mod, base .. "/init.lua" })
		elseif n:match("%.lua$") then
			out[#out + 1] = { mod .. "." .. n:sub(1, -5), base .. "/" .. n }
		elseif mare.fs.exists(base .. "/" .. n .. "/init.lua") then
			out[#out + 1] = { mod .. "." .. n, base .. "/" .. n .. "/init.lua" }
		end
	end
	return out
end

local function import(p)
	local mod = p.import
	if type(mod) ~= "string" or mod == "" then
		warn('mare.plugins.add: `import` is a module name under ~/.config/mare/lua ("packages")')
		return
	end
	local en = p.enabled
	if type(en) == "function" then
		local ok, v = pcall(en, p)
		en = ok and v ~= false and v ~= nil
	end
	if en == false then
		return
	end
	for _, m in ipairs(P.stack) do
		if m == mod then
			local chain = table.concat(P.stack, " → ") .. " → " .. mod
			warn(("mare.plugins.add: import cycle %s (skipped)"):format(chain))
			return
		end
	end
	if P.imported[mod] then
		return
	end
	if #P.stack >= MAX_IMPORT_DEPTH then
		warn(("mare.plugins.add: imports nested more than %d deep at %s (skipped)"):format(MAX_IMPORT_DEPTH, mod))
		return
	end
	P.imported[mod] = true
	P.stack[#P.stack + 1] = mod
	local count = 0
	for _, f in ipairs(module_files(mod)) do
		local chunk, err = loadfile(f[2])
		local ok, decl = false, err
		if chunk then
			ok, decl = pcall(chunk)
		end
		if ok and type(decl) == "table" then
			for _, d in ipairs(decl) do
				count = count + 1
				add_decl(d, nil, #P.stack)
			end
		elseif ok and decl == nil then
		-- a file that only calls mare.plugins.add itself
		else
			warn(("mare.plugins.add import %s (%s): %s"):format(mod, f[2], tostring(decl)))
		end
	end
	P.stack[#P.stack] = nil
	if count == 0 then
		warn(
			("mare.plugins.add: import %s found no package declarations (lua/%s.lua or lua/%s/*.lua)"):format(
				mod,
				mod:gsub("%.", "/"),
				mod:gsub("%.", "/")
			)
		)
	end
end

add_decl = function(p, needer, depth)
	depth = depth or 0
	if depth > MAX_DECL_DEPTH then
		warn("mare.plugins.add: declarations nested too deeply (a dependency that contains itself?)")
		return
	end
	if type(p) == "string" then
		p = { p }
	end
	if type(p) ~= "table" then
		warn('mare.plugins.add: each entry is a "owner/repo" string or a table')
		return
	end
	if p.import then
		return import(p)
	end
	local e = {}
	for k, v in pairs(p) do
		if type(k) == "string" then
			e[k] = v
		end
	end
	e.lock_scope = guard_option_write() == "project" and "project" or "user"
	-- the source is `[1]`; `name` is only the shown name (lazy.nvim) — or the id when there's no [1]
	local name = p[1] or p.name
	if p[1] and p.name and p.name ~= p[1] then
		e.display = p.name
	end
	if not name and p.dir then
		name = tostring(p.dir):match("([^/\\]+)[/\\]*$")
	end
	if type(name) ~= "string" then
		warn("mare.plugins.add: an entry without a name or dir")
		return
	end
	-- Keep each subdirectory's identity distinct while retaining the shared checkout source.
	local repo, sub = split_subdir(name)
	if sub then
		e.source = repo
		e.subdir = e.subdir or sub
		if p[1] and not p.name then
			name = repo .. ":" .. e.subdir
		end
	elseif e.subdir then
		e.source = name
		if p[1] and not p.name then
			name = name .. ":" .. e.subdir
		end
	end
	e.name = name
	-- enabled may be a function (per machine / environment), decided now
	if type(e.enabled) == "function" then
		local ok, v = pcall(e.enabled, e)
		e.enabled = ok and v ~= false and v ~= nil
	end
	if type(e.only) == "string" then
		e.only = { e.only }
	end
	if type(e.opts_extend) == "string" then
		e.opts_extend = { e.opts_extend }
	end
	local fresh = P.frags[name] == nil
	if fresh then
		P.frags[name] = {}
		P.order[#P.order + 1] = name
	end
	if needer then
		P.needed[name] = P.needed[name] or {}
		P.needed[name][needer] = true
	else
		P.explicit[name] = true
	end
	-- dependencies: names (declared too when they aren't; a table spec is a declaration)
	local deps = {}
	for _, d in ipairs(e.dependencies or {}) do
		local dn = type(d) == "string" and d or (type(d) == "table" and (d[1] or d.name)) or nil
		if type(dn) == "string" then
			if type(d) == "table" and d[1] and not d.name and d.subdir then
				local dep_repo, dep_sub = split_subdir(dn)
				dn = (dep_sub and dep_repo or dn) .. ":" .. d.subdir
			end
			deps[#deps + 1] = dn
			if type(d) == "table" then
				add_decl(d, name, depth + 1)
			elseif P.frags[dn] == nil then
				add_decl({ d }, name, depth + 1)
			else
				P.needed[dn] = P.needed[dn] or {}
				P.needed[dn][name] = true
			end
		end
	end
	e.dependencies = nil
	table.insert(P.frags[name], { fields = e, deps = deps })
	-- a live (pre-finalize) view: one entry per package in C.packages
	C.packages = C.packages or {}
	local prev = find_pkg(name)
	if prev then
		for k, v in pairs(e) do
			if k ~= "opts" then
				prev[k] = v
			end
		end
	else
		local live = {}
		for k, v in pairs(e) do
			live[k] = v
		end
		table.insert(C.packages, live)
	end
end

function mare.plugins.add(list)
	guard_option_write()
	C.packages = C.packages or {}
	if type(list) ~= "table" then
		warn("mare.plugins.add takes a list of declarations")
		return
	end
	for _, p in ipairs(list) do
		add_decl(p, nil, #P.stack)
	end
end

--- lazy.nvim `core/plugin.lua` `_values`: a function transforms the opts so far
--- (`opts(pkg, opts)`, returning nil keeps the table it got), a table deep-merges over them
--- (last write wins) except the `opts_extend` paths, whose lists are concatenated.
local function pkg_values(ret, values, extend, info)
	if values == nil then
		return ret
	end
	if type(values) == "function" then
		local ok, r = pcall(values, info, ret)
		if not ok then
			warn(("package %s: opts function failed: %s"):format(info.name, tostring(r)))
			return ret
		end
		if r == nil then
			return ret
		end
		return type(r) == "table" and r or ret
	end
	if type(values) ~= "table" then
		return ret
	end
	local lists = {}
	for _, key in ipairs(extend) do
		local path = split_dots(key)
		local r, v = key_get(ret, path), key_get(values, path)
		if type(r) == "table" and type(v) == "table" then
			local l = {}
			for _, x in ipairs(r) do
				l[#l + 1] = x
			end
			for _, x in ipairs(v) do
				l[#l + 1] = x
			end
			lists[#lists + 1] = { path = path, list = l }
		end
	end
	merge(ret, deepcopy(values))
	for _, l in ipairs(lists) do
		key_set(ret, l.path, l.list)
	end
	return ret
end

local function collect_package_specs()
	local by, dead, off = {}, {}, {}
	for _, id in ipairs(P.order) do
		local m, deps, only, extend, enabled, optional_all = {}, {}, nil, {}, true, true
		local seen_dep, seen_only, seen_ext = {}, {}, {}
		for _, f in ipairs(P.frags[id]) do
			local x = f.fields
			for k, v in pairs(x) do
				if k ~= "opts" and k ~= "opts_extend" and k ~= "only" and k ~= "enabled" and k ~= "optional" then
					m[k] = v
				end
			end
			if x.enabled ~= nil then
				enabled = x.enabled
			end
			if x.optional ~= true then
				optional_all = false
			end
			for _, d in ipairs(f.deps) do
				if not seen_dep[d] then
					seen_dep[d] = true
					deps[#deps + 1] = d
				end
			end
			if type(x.only) == "table" then
				only = only or {}
				for _, o in ipairs(x.only) do
					if not seen_only[o] then
						seen_only[o] = true
						only[#only + 1] = o
					end
				end
			end
			for _, k in ipairs(type(x.opts_extend) == "table" and x.opts_extend or {}) do
				if not seen_ext[k] then
					seen_ext[k] = true
					extend[#extend + 1] = k
				end
			end
		end
		m.name = id
		m.dependencies = #deps > 0 and deps or nil
		m.only = only
		m._extend = extend
		if optional_all then
			dead[id] = true
		end
		if not enabled then
			off[id] = true
		end
		by[id] = m
	end
	return by, dead, off
end

local function propagate_disabled_deps(dead, off)
	-- what only dropped or disabled packages need is dropped / disabled with them
	local changed = true
	while changed do
		changed = false
		for _, id in ipairs(P.order) do
			if not P.explicit[id] and P.needed[id] and not dead[id] and not off[id] then
				local live, any_off = false, false
				for parent in pairs(P.needed[id]) do
					if not dead[parent] and not off[parent] then
						live = true
					end
					if off[parent] then
						any_off = true
					end
				end
				if not live then
					if any_off then
						off[id] = true
					else
						dead[id] = true
					end
					changed = true
				end
			end
		end
	end
end

-- each enabled package's merged opts, waiting for state.merge_package_opts
local pending_opts = {}

--- Merge the fragments into C.packages. Called once per evaluation, after the config files ran;
--- the enabled packages' opts follow through state.merge_package_opts once the plugins those
--- packages ship have declared their options.
function state.finalize_packages()
	pending_opts = {}
	if #P.order == 0 then
		return
	end
	local by, dead, off = collect_package_specs()
	propagate_disabled_deps(dead, off)
	local out = {}
	for _, id in ipairs(P.order) do
		local m = by[id]
		if not dead[id] then
			if off[id] then
				m.enabled = false
				m.opts = nil
			else
				m.enabled = nil
				local info = { name = id, display = m.display, dir = m.dir, subdir = m.subdir }
				local acc = {}
				for _, f in ipairs(P.frags[id]) do
					acc = pkg_values(acc, f.fields.opts, m._extend, info)
				end
				m.opts = next(acc) ~= nil and acc or nil
				if m.opts then
					pending_opts[#pending_opts + 1] = { id = id, opts = acc }
				end
			end
			m._extend = nil
			out[#out + 1] = m
		end
	end
	C.packages = out
end

--- The enabled packages' opts as one layer `{ <plugin> = opts }`, for the host to put under the
--- final configuration's `plugins` (every other layer wins). Each package's opts are first checked
--- against the declarations like every other entry point: an invalid value is reported, naming the
--- package, and taken out (or fails its plugin, as the declaration says), never merged unchecked.
function state.merge_package_opts()
	local check = state.check_config
	check(nil, nil, true)
	local layer = {}
	for _, p in ipairs(pending_opts) do
		check({ plugins = p.opts }, "package " .. p.id)
		for plugin, o in pairs(p.opts) do
			if type(plugin) == "string" and type(o) == "table" then
				layer[plugin] = layer[plugin] or {}
				merge(layer[plugin], deepcopy(o))
			end
		end
	end
	pending_opts = {}
	return layer
end
