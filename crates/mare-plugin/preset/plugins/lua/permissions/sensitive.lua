-- Credential path reminders narrow the existing permission vote; they are not a sandbox.
return function(D)
	local cfg = D.cfg.sensitive or {}
	local off = {}
	for _, id in ipairs(cfg.disable or {}) do
		off[id] = true
	end
	for _, pattern in ipairs(cfg.patterns or {}) do
		local globs, err = D.rules.path_globs(pattern)
		assert(globs, "invalid sensitive pattern: " .. tostring(err))
	end
	local dirs = {
		{ "ssh_dir", ".ssh" },
		{ "aws_dir", ".aws" },
		{ "azure_dir", ".azure" },
		{ "gnupg_dir", ".gnupg" },
		{ "kube_dir", ".kube" },
		{ "docker_dir", ".docker" },
		{ "gcloud_dir", ".config/gcloud" },
		{ "gh_dir", ".config/gh" },
		{ "gcp_dir", ".gcp" },
	}
	local function equal(a, b)
		return mare.fs.path_within(a, b, true) and mare.fs.path_within(b, a, true)
	end
	local function within(path, root, recursive)
		return mare.fs.path_within(path, root, true) or (recursive and mare.fs.path_within(root, path, true))
	end
	local function basename(path)
		local name = path:match(mare.os == "windows" and "([^/\\]+)$" or "([^/]+)$") or path
		return (mare.os == "windows" or mare.os == "macos") and name:lower() or name
	end
	local function basename_hit(name)
		if
			not off.env
			and (name == ".env" or name:match("^%.env%."))
			and name ~= ".env.example"
			and name ~= ".env.sample"
			and name ~= ".env.template"
		then
			return true
		end
		if not off.ssh_private_keys then
			for _, key in ipairs({ "id_rsa", "id_ed25519", "id_ecdsa" }) do
				if name ~= key .. ".pub" and (name == key or name:match("^" .. key .. "[-_%.]")) then
					return true
				end
			end
		end
		return (not off.netrc and name == ".netrc")
			or (not off.git_credentials and name == ".git-credentials")
			or (not off.npmrc and name == ".npmrc")
			or (not off.pypirc and name == ".pypirc")
	end
	local function dirname(p)
		if mare.os == "windows" then
			local d = p:match("^(.*)[/\\][^/\\]+$")
			if d and d:match("^%a:$") then
				return d .. "\\"
			end
			return d
		end
		local d = p:match("^(.*)/[^/]+$")
		if d == "" and p:sub(1, 1) == "/" then
			return "/"
		end
		return d
	end
	local function roots(path)
		if not path then
			return {}
		end
		local ok, resolved = pcall(mare.fs.resolve_path, path)
		if ok and type(resolved) == "table" then
			if resolved.logical == resolved.physical then
				return { resolved.logical }
			end
			return { resolved.logical, resolved.physical }
		end
		local logical = mare.fs.resolve(path)
		local out = { logical }
		local cur = dirname(logical)
		while cur and cur ~= "" do
			local pok, pres = pcall(mare.fs.resolve_path, cur)
			if pok and type(pres) == "table" then
				local tail = D.rules.path_suffix_after(logical, cur)
				if tail then
					local phys = pres.physical .. tail
					if phys ~= logical then
						out[#out + 1] = phys
					end
				end
				break
			end
			local parent = dirname(cur)
			if not parent or parent == cur then
				break
			end
			cur = parent
		end
		return out
	end
	local function hit(path, recursive, home, state, auth)
		if basename_hit(basename(path)) then
			return true
		end
		for _, home_root in ipairs(home or {}) do
			for _, dir in ipairs(dirs) do
				if not off[dir[1]] and within(path, home_root .. "/" .. dir[2], recursive) then
					return true
				end
			end
		end
		for _, auth_path in ipairs(auth or {}) do
			if not off.mare_auth and (equal(path, auth_path) or equal(path, auth_path .. ".bak")) then
				return true
			end
		end
		for _, state_root in ipairs(state or {}) do
			if not off.mare_mcp_oauth and within(path, state_root .. "/mcp-oauth", recursive) then
				return true
			end
			if not off.mare_host_token then
				local host = state_root .. "/host"
				local name = basename(path)
				if
					equal(path, host .. "/token")
					or equal(path, host .. "/token.bak")
					or (within(path, host, false) and name:match("^%.token%..*%.tmp$"))
				then
					return true
				end
			end
		end
		for _, pattern in ipairs(cfg.patterns or {}) do
			local globs = assert(D.rules.path_globs(pattern))
			for _, glob in ipairs(globs) do
				if (recursive and glob:overlaps(path)) or (not recursive and glob:matches(path, true)) then
					return true
				end
			end
		end
		return false
	end
	local function collect(call, info, repr, parsed)
		if cfg.enabled == false or info.unconfined or (D.presets.LIKE[D.P.preset] or D.P.preset) == "full-access" then
			return nil
		end
		local home, state, auth = roots(mare.fs.home), roots(mare.paths.state), roots(mare.paths.auth)
		local found = {}
		local function consider(item, unresolved)
			local sensitive = unresolved
				or hit(item.logical, item.recursive, home, state, auth)
				or hit(item.physical, item.recursive, home, state, auth)
			if sensitive and (unresolved or not D.rules.sensitive_exempt(call, item, info.tool_version)) then
				found[#found + 1] = item
			end
		end
		if info.access_state == "declared" and type(info.access) == "table" then
			for _, item in ipairs(info.access) do
				consider(item, false)
			end
		end
		-- These are advisory argv candidates, never an access declaration or a path-rule allow.
		if info.command_tool and repr and (repr.text or repr.argv) then
			if not (type(parsed) == "table" and parsed.literal_paths ~= nil) then
				local input = repr.kind == "argv" and repr.argv or repr.text
				parsed = mare.text.shell_parse(
					input,
					{ shell = repr.shell or "bash", safe_sinks = (D.cfg.parse or {}).safe_sinks }
				)
			end
			for _, candidate in ipairs(parsed.literal_paths or {}) do
				local ok, path = pcall(mare.fs.resolve_path, candidate, call.cwd or mare.cwd)
				local item = {
					shell = true,
					op = nil,
					path = candidate,
					recursive = false,
					logical = ok and path.logical or candidate,
					physical = ok and path.physical or candidate,
				}
				consider(item, not ok)
			end
		end
		return #found > 0 and found or nil
	end
	return { collect = collect }
end
