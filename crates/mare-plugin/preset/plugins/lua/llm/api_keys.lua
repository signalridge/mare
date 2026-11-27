-- llm.api_keys: init.lua provider settings, API key lookup (key helper, env, auth.json), the
-- doctor checks and per-provider request headers; part of the `llm` plugin (plugins/llm.lua), loaded at the start of its apply.
return function(D)
	local ctx = D.ctx
	local providers, models, state, llm = D.providers, D.models, D.state, D.llm
	local read_auth = D.auth_store.read_auth

	--- Effective provider settings: materialized directory entry (base + overlay) for data-tier
	--- providers, or `mare.config.providers[name]` for code-backed providers.
	local function provider_cfg(name)
		if D.directory and D.directory.effective_config then
			return D.directory.effective_config(name)
		end
		return (mare.config.providers or {})[name] or {}
	end
	function llm.base_url(_, _, name, default)
		return provider_cfg(name).base_url or default
	end

	local key_cache = {}
	local key_locks = {}
	local value_cache = {}
	local provider_value_keys = {}
	--- invalidate_api_key(provider): the next api_key() runs the key helper again (after a 401)
	function llm.invalidate_api_key(_, _, name)
		key_cache[name] = nil
		local keys = provider_value_keys[name]
		if keys then
			for ck in pairs(keys) do
				value_cache[ck] = nil
			end
			provider_value_keys[name] = nil
		end
	end
	local function is_env_name(s)
		return type(s) == "string" and string.match(s, "^[A-Z][A-Z0-9_]*$") ~= nil
	end

	--- api_key(provider, default_env_names) -> key | nil
	--- order: init.lua api_key (ENV_NAME | {cmd = {...}} | literal) → auth.json
	--- (/login) → the provider's default environment variables (lowest)
	local known_envs = {} -- provider -> its default env names (mare doctor's source listing)
	function llm.api_key(_, _, name, envs)
		if envs then
			known_envs[name] = known_envs[name] or envs
		end
		local k = provider_cfg(name).api_key
		if type(k) == "table" and k.cmd then
			-- key helper (apiKeyHelper:28-70): one run at
			-- a time (later callers wait for it), cached until `refresh_interval_ms` passes or the
			-- helper's own `expires_at` (JSON output {key|token, expires_at}); a 401 drops the cache
			-- (invalidate_api_key) so the next call runs it again
			local l = key_locks[name]
			if not l then
				l = require("mare_lib").mutex()
				key_locks[name] = l
			end
			local res = l:with(function()
				local c = key_cache[name]
				local now = mare.task.now()
				if c and c.expires and now >= c.expires then
					c = nil
				end
				if c == nil then
					local cmd = k.cmd
					local r = mare.proc.run({ cmd = cmd[1], args = { table.unpack(cmd, 2) }, timeout_ms = 30000 })
					local out = r.success and string.match(r.stdout or "", "^%s*(.-)%s*$") or ""
					local key, expires = out, nil
					local j = out:sub(1, 1) == "{" and mare.json.decode(out) or nil
					if type(j) == "table" then
						key = j.key or j.token or j.access_token or ""
						expires = tonumber(j.expires_at) and tonumber(j.expires_at) * 1000 or nil
					end
					if k.refresh_interval_ms then
						local by = now + k.refresh_interval_ms
						expires = expires and math.min(expires, by) or by
					end
					if key == "" then
						mare.log("warn", ("api_key command for %s failed: %s"):format(name, r.stderr or ""))
						c = { key = false, expires = now + 5000 } -- don't hammer a broken helper
					else
						c = { key = key, expires = expires }
					end
					key_cache[name] = c
				end
				return c.key
			end)
			if res then
				return res
			end
		elseif is_env_name(k) then
			local v = mare.env(k)
			if v and v ~= "" then
				return v
			end
		elseif type(k) == "string" and k ~= "" then
			return k
		end
		local a = llm.auth_get(llm, nil, name)
		if a and a.key then
			return a.key
		end
		for _, e in ipairs(envs or {}) do
			local v = mare.env(e)
			if v and v ~= "" then
				return v
			end
		end
		return nil
	end

	--- api_key_source(provider, default_env_names) -> description of where the key would come
	--- from ("init.lua command", "init.lua literal", "env NAME (init.lua)", "auth.json",
	--- "env NAME"), or nil; never the secret and never runs a command (mare doctor)
	function llm.api_key_source(_, _, name, envs)
		local k = provider_cfg(name).api_key
		if type(k) == "table" and k.cmd then
			return "init.lua command (" .. tostring(k.cmd[1] or "?") .. ")"
		elseif is_env_name(k) then
			if (mare.env(k) or "") ~= "" then
				return "env " .. k .. " (init.lua)"
			end
		elseif type(k) == "string" and k ~= "" then
			return "init.lua literal"
		end
		local a = llm.auth_get(llm, nil, name)
		if a and (a.key or a.access or a.refresh) then
			return a.key and "auth.json" or "auth.json (OAuth)"
		end
		for _, e in ipairs(envs or {}) do
			if (mare.env(e) or "") ~= "" then
				return "env " .. e
			end
		end
		return nil
	end
	-- mare doctor: where each provider's credential comes from (never the secret), and
	-- connectivity for the configured providers only (a credential, an init.lua
	-- `mare.provider` entry, or the current model's provider) — never a fixed list
	ctx:on("kernel/doctor", function(checks)
		local names, seen = {}, {}
		local function add(n)
			if n and not seen[n] then
				seen[n] = true
				names[#names + 1] = n
			end
		end
		for n in pairs(known_envs) do
			add(n)
		end
		table.sort(names)
		local parts, configured = {}, {}
		local corrupt
		for _, n in ipairs(names) do
			local okc, src = pcall(llm.api_key_source, llm, nil, n, known_envs[n])
			if not okc then
				corrupt = corrupt or tostring(src)
				src = nil
			end
			if src then
				configured[#configured + 1] = n
			end
			parts[#parts + 1] = n .. ": " .. (src or "none")
		end
		local _, rerr = read_auth()
		corrupt = corrupt or rerr
		local detail = #configured > 0 and table.concat(parts, "; ")
			or "none — run /login or set a provider's API key variable"
		if corrupt then
			detail = corrupt .. "; " .. detail
		end
		table.insert(checks, { name = "credential sources", ok = #configured > 0 and not corrupt, detail = detail })
		-- init.lua explicit api_key > stored logins > environment variables; a
		-- source that is set but not used is easy to misread, so it is named with a warning
		local overridden = {}
		local okl, lines = pcall(llm.status_lines, llm, ctx)
		for _, l in ipairs(okl and lines or {}) do
			if l.warning then
				overridden[#overridden + 1] = (l.text:gsub("^%s*⚠%s*", ""))
			end
		end
		if #overridden > 0 then
			table.insert(checks, {
				name = "credential overrides",
				ok = true,
				warn = true,
				detail = table.concat(overridden, "; "),
			})
		end

		local cur = state.model and models[state.model]
		local probe = {}
		for _, n in ipairs(configured) do
			probe[n] = true
		end
		for n in pairs(mare.config.providers or {}) do
			probe[n] = true
		end
		if cur then
			probe[cur.provider] = true
		end
		local list = {}
		for n in pairs(probe) do
			list[#list + 1] = n
		end
		table.sort(list)
		for _, n in ipairs(list) do
			local p = providers[n]
			local url = provider_cfg(n).base_url or (p and p.warm_url and p.warm_url())
			if url then
				local ok, r = pcall(mare.http.request, { method = "GET", url = url, timeout_ms = 5000 })
				table.insert(checks, {
					name = "reach " .. n,
					ok = ok,
					detail = ok and ("%s reachable (HTTP %s)"):format(url, tostring(r.status)) or tostring(r),
				})
			end
		end
	end)

	--- like api_key but never runs a command (for cheap "is configured" checks)
	function llm.has_api_key(_, _, name, envs)
		known_envs[name] = known_envs[name] or envs or {}
		local k = provider_cfg(name).api_key
		if type(k) == "table" and k.cmd then
			return not (key_cache[name] and key_cache[name].key == false)
		end
		if is_env_name(k) then
			if (mare.env(k) or "") ~= "" then
				return true
			end
		elseif type(k) == "string" and k ~= "" then
			return true
		end
		if llm.auth_get(llm, nil, name) ~= nil then
			return true
		end
		for _, e in ipairs(envs or {}) do
			if (mare.env(e) or "") ~= "" then -- an empty variable is unset, as api_key reads it
				return true
			end
		end
		return false
	end

	--- has_auth(key) -> whether auth.json holds a credential under `key`; never the value, so
	--- readiness and status checks need no access to secrets
	function llm.has_auth(_, _, name)
		return llm.auth_get(llm, nil, name) ~= nil
	end

	--- a value from init.lua that may name an environment variable or a command (headers,
	--- "值同样可以是环境变量或命令":20)
	local function resolve_value(v, provider)
		if type(v) == "table" and v.cmd then
			local ck = mare.json.encode(v.cmd)
			if provider then
				provider_value_keys[provider] = provider_value_keys[provider] or {}
				provider_value_keys[provider][ck] = true
			end
			local cached = value_cache[ck]
			if cached and (not cached.expires or mare.task.clock() < cached.expires) then
				return cached.value
			end
			local cmd = v.cmd
			local r = mare.proc.run({ cmd = cmd[1], args = { table.unpack(cmd, 2) }, timeout_ms = 30000 })
			if r.success then
				local val = string.match(r.stdout or "", "^%s*(.-)%s*$")
				if val ~= "" then
					local ttl = tonumber(v.refresh_interval_ms)
					local expires = (ttl and ttl > 0) and (mare.task.clock() + ttl) or nil
					value_cache[ck] = { value = val, expires = expires }
					return val
				end
			end
			-- Never cache failed/empty command results so the next request can recover.
			value_cache[ck] = nil
			return nil
		elseif type(v) == "table" and v.env then
			return mare.env(v.env)
		elseif is_env_name(v) then
			local e = mare.env(v)
			return (e and e ~= "") and e or nil
		end
		return v ~= nil and tostring(v) or nil
	end
	local function has_cmd_auth(provider)
		local pc = provider_cfg(provider)
		if type(pc.api_key) == "table" and pc.api_key.cmd then
			return true
		end
		for _, tbl in ipairs({ pc.headers, pc.query_params }) do
			if type(tbl) == "table" then
				for _, v in pairs(tbl) do
					if type(v) == "table" and v.cmd then
						return true
					end
				end
			end
		end
		return false
	end
	-- every provider's wire request passes through `provider/request` (a waterfall: this
	-- listener is around-form, `fn(wire, next)`, and passes the wire on): add the provider's
	-- init.lua headers and query parameters, and its proxy / connect timeout for mare.http
	local function configure(wire)
		if type(wire) ~= "table" or type(wire.provider) ~= "string" then
			return wire
		end
		local pc = provider_cfg(wire.provider)
		if type(pc.headers) == "table" then
			wire.headers = wire.headers or {}
			for k, v in pairs(pc.headers) do
				wire.headers[k] = resolve_value(v, wire.provider)
			end
		end
		if type(pc.query_params) == "table" and type(wire.url) == "string" then
			local q = {}
			for k, v in pairs(pc.query_params) do
				q[k] = resolve_value(v, wire.provider)
			end
			local base, existing = wire.url:match("^([^?]*)%??(.*)$")
			local built = mare.http.build_url(base, q)
			if existing ~= "" then
				built = built .. (built:find("?", 1, true) and "&" or "?") .. existing
			end
			wire.url = built
		end
		wire.proxy = wire.proxy or pc.proxy
		wire.connect_timeout_ms = wire.connect_timeout_ms or pc.connect_timeout_ms
		return wire
	end
	ctx:on("provider/request", function(wire, ev)
		return ev.next(configure(wire))
	end)
	--- provider_config(name) -> its mare.provider table (read-only use)
	function llm.provider_config(_, _, name)
		return provider_cfg(name)
	end

	return {
		provider_cfg = provider_cfg,
		key_cache = key_cache,
		is_env_name = is_env_name,
		known_envs = known_envs,
		has_cmd_auth = has_cmd_auth,
	}
end
