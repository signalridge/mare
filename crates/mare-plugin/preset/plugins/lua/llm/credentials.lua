-- llm.credentials: credential sources, login hints, login status and status lines;
-- part of the `llm` plugin (plugins/llm.lua), loaded at the start of its apply.
local lib = require("mare_lib")

return function(D)
	local providers, state, llm = D.providers, D.state, D.llm
	local read_auth = D.auth_store.read_auth
	local provider_cfg, is_env_name, known_envs = D.api_keys.provider_cfg, D.api_keys.is_env_name, D.api_keys.known_envs

	-- One owner per auth.json key in this VM; other processes coordinate through the file lock.
	local refreshes = {}
	function llm.refresh_credential(_, caller, _provider, key, refresh_fn, opts)
		opts = opts or {}
		local initial = llm.auth_get(llm, caller, key)
		if not initial then
			return nil
		end
		local function valid(credential)
			return credential and opts.valid and opts.valid(credential)
		end
		local function same_account(a, b)
			return not opts.same_account or opts.same_account(a, b)
		end
		local function account_changed_error()
			return {
				message = "Your access token could not be refreshed because you have since logged out or signed in to another account. Please sign in again.",
				kind = "auth",
			}
		end
		if not opts.force and valid(initial) then
			return initial
		end
		if opts.cancel and opts.cancel:is_cancelled() then
			return nil, "OAuth preparation cancelled before dispatch"
		end

		local function run_flight(force_flight)
			local flight = refreshes[key]
			if not flight then
				flight = {
					done = mare.task.token(),
					admission = mare.task.token(),
					waiters = {},
					force = force_flight,
				}
				refreshes[key] = flight
				local spawned, owner = pcall(mare.task.spawn, function()
					local guard
					local release <close> = setmetatable({}, {
						__close = function()
							if guard then
								guard:release()
							end
						end,
					})
					local function has_waiter()
						for waiter in pairs(flight.waiters) do
							if not waiter.cancel or not waiter.cancel:is_cancelled() then
								return true
							end
						end
						return false
					end
					local ok, result = pcall(function()
						guard = mare.lock.file(
							mare.paths.auth .. ".lock",
							{ cancel = flight.admission, timeout_ms = 30000 }
						)
						if not has_waiter() then
							error("OAuth preparation cancelled before dispatch", 0)
						end
						local current = llm.auth_get(llm, caller, key)
						if not current then
							return nil
						end
						if not same_account(initial, current) then
							error(account_changed_error(), 0)
						end
						local refreshed
						if opts.refreshed then
							refreshed = opts.refreshed(initial, current)
						else
							refreshed = not D.auth_store.same(initial, current)
						end
						if valid(current) and (not flight.force or refreshed) then
							return current
						end
						if not has_waiter() then
							error("OAuth preparation cancelled before dispatch", 0)
						end
						flight.dispatched = true
						local next_credential = refresh_fn(current)
						if
							not llm.auth_set(llm, caller, key, next_credential, { locked = true, expected = current })
						then
							local latest = llm.auth_get(llm, caller, key)
							if latest and same_account(current, latest) and valid(latest) then
								return latest
							end
							error({ message = "Account credentials changed during refresh.", kind = "auth" }, 0)
						end
						return next_credential
					end)
					flight.ok, flight.result = ok, result
					if refreshes[key] == flight then
						refreshes[key] = nil
					end
					flight.done:cancel()
				end)
				if not spawned then
					refreshes[key] = nil
					return nil, owner
				end
				flight.owner = owner
			elseif force_flight then
				flight.force = true
			end

			local waiter = { cancel = opts.cancel }
			flight.waiters[waiter] = true
			local withdraw <close> = setmetatable({}, {
				__close = function()
					flight.waiters[waiter] = nil
					if not next(flight.waiters) and not flight.dispatched then
						if refreshes[key] == flight then
							refreshes[key] = nil
						end
						flight.admission:cancel()
					end
				end,
			})
			if opts.cancel then
				local selected = mare.task.select(function()
					flight.done:wait()
				end, function()
					opts.cancel:wait()
				end)
				if selected == 2 then
					return nil, "OAuth preparation cancelled before dispatch"
				end
			else
				flight.done:wait()
			end
			if not flight.ok then
				return nil, flight.result
			end
			if flight.result and not same_account(initial, flight.result) then
				return nil, account_changed_error()
			end
			return flight.result
		end

		local result, err = run_flight(opts.force == true)
		if err ~= nil then
			return nil, err
		end
		if opts.force and result then
			local rotated = opts.refreshed and opts.refreshed(initial, result) or not D.auth_store.same(initial, result)
			if not rotated then
				return run_flight(true)
			end
		end
		return result
	end

	---------------------------------------------------------------------------
	-- login, logout and credential status
	---------------------------------------------------------------------------
	--- an account or key shown masked: "ab***@example.com", "sk-…1234"
	local function mask(s)
		s = tostring(s or "")
		local u, d = s:match("^([^@]*)@(.+)$")
		if u then
			return u:sub(1, 2) .. "***@" .. d
		end
		if #s <= 8 then
			return "****"
		end
		return s:sub(1, 3) .. "…" .. s:sub(-4)
	end
	--- auth.json keys a provider owns (`auth_keys`; default its own name): logging out of
	--- `openai` removes the API key and the ChatGPT login (`openai-codex`)
	local function auth_keys(name)
		local p = providers[name]
		return (p and p.auth_keys) or { name }
	end
	--- collect_sources(provider) -> every credential present for it in priority order:
	--- 1. explicit init.lua api_key (command | ENV_NAME | literal)
	--- 2. stored logins in auth.json (OAuth login first among stored logins, then other keys)
	--- 3. provider default environment variables
	--- Never runs a key command. Each row carries internal `_tier` ("config"|"auth"|"env") stripped
	--- from public `credential_sources` results.
	local function collect_sources(name)
		local p = providers[name]
		local auth, err = read_auth()
		if not auth then
			error(err, 0)
		end
		local out = {}
		local function add(e)
			e.active = #out == 0
			out[#out + 1] = e
		end
		local k = provider_cfg(name).api_key
		if type(k) == "table" and k.cmd then
			add({
				source = "init.lua command (" .. tostring(k.cmd[1] or "?") .. ")",
				kind = "command",
				_tier = "config",
			})
		elseif is_env_name(k) then
			local v = mare.env(k)
			if v and v ~= "" then
				add({ source = "env " .. k .. " (init.lua)", kind = "api_key", account = mask(v), _tier = "config" })
			end
		elseif type(k) == "string" and k ~= "" then
			add({ source = "init.lua", kind = "api_key", account = mask(k), _tier = "config" })
		end
		-- stored logins in auth.json: an OAuth login is preferred over a stored API key when both
		-- exist in auth.json, unless an explicit init.lua api_key overrides stored credentials.
		local ok_key = p and p.oauth_key
		local o = ok_key and auth[ok_key]
		if type(o) == "table" and (o.access or o.refresh) then
			add({
				source = "auth.json (" .. ok_key .. ")",
				kind = "oauth",
				key = ok_key,
				account = (o.email and mask(o.email)) or (o.account_id and mask(o.account_id)) or nil,
				expires_at_ms = lib.to_epoch_ms(tonumber(o.expires)),
				_tier = "auth",
			})
		end
		for _, key in ipairs(auth_keys(name)) do
			local a = auth[key]
			if key ~= ok_key and type(a) == "table" and (a.key or a.access or a.refresh) then
				add({
					source = key == name and "auth.json" or ("auth.json (" .. key .. ")"),
					kind = a.key and "api_key" or "oauth",
					key = key,
					account = (a.key and mask(a.key)) or (a.email and mask(a.email)) or nil,
					expires_at_ms = lib.to_epoch_ms(tonumber(a.expires)),
					_tier = "auth",
				})
			end
		end
		for _, e in ipairs(known_envs[name] or {}) do
			local v = mare.env(e)
			if v and v ~= "" then
				add({ source = "env " .. e, kind = "api_key", account = mask(v), _tier = "env" })
			end
		end
		return out
	end

	--- credential_sources(provider) -> every credential present for it, the one in use first:
	--- { source, kind = "oauth"|"api_key"|"command", account?, expires_at_ms?, key?, active }.
	--- Never runs a key command. Order: init.lua api_key > auth.json > env; the
	--- rest are listed as overridden so `/status` and `mare doctor` can say so.
	function llm.credential_sources(_, _, name)
		local out = collect_sources(name)
		for _, e in ipairs(out) do
			e._tier = nil
		end
		return out
	end

	--- active_credential(provider, envs?) -> the winning credential descriptor using the unified
	--- priority (init.lua > auth.json > default envs), or nil when unconfigured. Never runs a command.
	function llm.active_credential(_, _, name, envs)
		if type(envs) == "table" and not known_envs[name] then
			known_envs[name] = envs
		end
		local first = collect_sources(name)[1]
		if not first then
			return nil
		end
		return {
			source = first.source,
			kind = first.kind,
			tier = first._tier,
			key = first.key,
		}
	end

	--- Internal unmasked credential fingerprint for cache isolation (never exposes secrets).
	function D.credential_scope(name)
		local p = providers[name]
		local auth, err = read_auth()
		if not auth then
			error(err, 0)
		end
		local parts = {}
		local ok_key = p and p.oauth_key
		local o = ok_key and auth[ok_key]
		if type(o) == "table" and (o.access or o.refresh) then
			parts[#parts + 1] = mare.crypto.sha256(mare.json.encode_strict({
				source = "oauth:" .. ok_key,
				account_id = o.account_id,
				email = o.email,
				refresh = o.refresh,
				access = o.access,
			}))
		end
		local k = provider_cfg(name).api_key
		if type(k) == "table" and k.cmd then
			-- Never execute a key command during passive cache-scope checks (e.g. startup):
			-- unresolved cmd credentials return nil so disk caches are skipped until resolved.
			local c = D.api_keys and D.api_keys.key_cache and D.api_keys.key_cache[name]
			if c and c.expires and mare.task.now() >= c.expires then
				c = nil
			end
			local resolved = c and c.key or nil
			if type(resolved) == "string" and resolved ~= "" then
				parts[#parts + 1] =
					mare.crypto.sha256(mare.json.encode_strict({ source = "cmd", cmd = k.cmd, key = resolved }))
			else
				return nil
			end
		elseif is_env_name(k) then
			local v = mare.env(k)
			if v and v ~= "" then
				parts[#parts + 1] = mare.crypto.sha256("env_cfg:" .. k .. ":" .. v)
			end
		elseif type(k) == "string" and k ~= "" then
			parts[#parts + 1] = mare.crypto.sha256("init:" .. k)
		end
		for _, key in ipairs(auth_keys(name)) do
			local a = auth[key]
			if key ~= ok_key and type(a) == "table" and (a.key or a.access or a.refresh) then
				parts[#parts + 1] = mare.crypto.sha256(mare.json.encode_strict({
					source = "auth:" .. key,
					key = a.key,
					email = a.email,
					refresh = a.refresh,
					access = a.access,
				}))
			end
		end
		for _, e in ipairs(known_envs[name] or {}) do
			local v = mare.env(e)
			if v and v ~= "" then
				parts[#parts + 1] = mare.crypto.sha256("env:" .. e .. ":" .. v)
			end
		end
		if #parts == 0 then
			return nil
		end
		return mare.crypto.sha256(table.concat(parts, "|"))
	end

	local function how_to_login(name)
		local hint = provider_cfg(name).login_hint
		if type(hint) == "string" and hint ~= "" then
			return hint
		end
		local envs = known_envs[name] or {}
		local login = mare.mode == "tui" and ("/login " .. name) or ("`mare login " .. name .. "`")
		if #envs > 0 then
			return ("run %s or set %s"):format(login, table.concat(envs, " / "))
		end
		return "run " .. login
	end
	-- shared by the llm modules; not part of the service
	D.how_to_login = how_to_login

	--- login_status() -> one row per known provider (live, disabled, failed, or unresolved):
	--- { provider, label, logged_in, state, reason?, plugin?, origin?, wire?, method?, account?,
	--- expires?, source?, overridden = { source, ... }, hint? }
	local function login_status(caller)
		local rows = {}
		local names = D.directory and D.directory.known_names and D.directory.known_names() or nil
		if not names then
			names = {}
			for _, p in ipairs(D.all_providers()) do
				names[#names + 1] = p.name
			end
		end
		for _, name in ipairs(names) do
			local st = D.directory and D.directory.status and D.directory.status(name) or nil
			local p = providers[name]
			if p ~= nil then
				-- asking the provider first registers its default variable names
				local configured = p.is_configured and p.is_configured()
				local srcs = collect_sources(p.name)
				local a = srcs[1]
				local row = {
					provider = p.name,
					label = p.label or p.name,
					logged_in = a ~= nil or (p.no_key == true),
					no_key = p.no_key == true or nil,
					overridden = {},
					state = "registered",
					origin = st and st.origin or "plugin",
					plugin = st and st.plugin or nil,
					wire = (st and st.wire) or p.wire,
				}
				if a then
					row.method = a.kind == "oauth" and (p.oauth_label or "OAuth")
						or (a.kind == "command" and "key command" or "API key")
					row.account = a.account
					row.expires_at_ms = a.expires_at_ms
					row.source = a.source
					row.tier = a._tier
					for i = 2, #srcs do
						row.overridden[#row.overridden + 1] = srcs[i].source
					end
				elseif not p.no_key and configured then
					-- configured some other way (the provider's own options): say so, don't guess
					row.logged_in, row.method = true, "configured"
				end
				if not row.logged_in then
					row.hint = how_to_login(p.name)
				end
				rows[#rows + 1] = row
			elseif st ~= nil then
				-- Non-registered directory/declared entries are never loginable, never execute key
				-- commands, never return secrets, and never count toward `has_credentials`.
				rows[#rows + 1] = {
					provider = name,
					label = name,
					logged_in = false,
					overridden = {},
					state = st.state,
					reason = st.reason,
					plugin = st.plugin,
					origin = st.origin,
					wire = st.wire,
				}
			end
		end
		return rows
	end
	--- status_lines() -> human-readable login status, one line per provider (/status, `mare
	--- login status`, doctor), plus a warning line for every overridden source
	function llm.status_lines(_, caller)
		local out = {}
		for _, r in ipairs(login_status(caller)) do
			if r.state and r.state ~= "registered" then
				out[#out + 1] = {
					text = ("%s: %s"):format(r.provider, r.reason or r.state),
					provider = r.provider,
					logged_in = false,
					state = r.state,
					unavailable = true,
				}
			else
				local line
				if r.no_key then
					line = ("%s: no key needed"):format(r.provider)
				elseif r.logged_in then
					local bits = {}
					if r.account then
						bits[#bits + 1] = r.account
					end
					if r.source then
						bits[#bits + 1] = r.source
					end
					if r.expires_at_ms then
						local left = r.expires_at_ms - mare.task.now()
						bits[#bits + 1] = left > 0
								and ("expires " .. os.date("%Y-%m-%d %H:%M", r.expires_at_ms // 1000))
							or "expired (refreshed on the next request)"
					end
					line = ("%s: %s%s"):format(
						r.provider,
						r.method,
						#bits > 0 and (" (" .. table.concat(bits, ", ") .. ")") or ""
					)
				else
					line = ("%s: not logged in — %s"):format(r.provider, r.hint)
				end
				out[#out + 1] = { text = line, provider = r.provider, logged_in = r.logged_in, state = "registered" }
				if #r.overridden > 0 then
					local rule = r.tier == "config" and "init.lua wins"
						or ("stored logins win; /logout %s to use it"):format(r.provider)
					out[#out + 1] = {
						text = ("  ⚠ %s also has %s — not used while %s is set (%s)"):format(
							r.provider,
							table.concat(r.overridden, ", "),
							r.source or "the first",
							rule
						),
						provider = r.provider,
						warning = true,
					}
				end
			end
		end
		return out
	end

	--- has_credentials() -> true when some provider can be used: a credential (key, login,
	--- key command), or the current model's provider is keyless or has no login of its own
	--- (onboarding trigger)
	function llm.has_credentials(_, caller)
		for _, p in ipairs(D.all_providers()) do
			if not p.no_key and (not p.is_configured or p.is_configured()) then
				return true
			end
		end
		-- the current model's provider needs no key, or can't be logged into from mare
		-- (plugin-defined providers): the guide couldn't help
		local cur = state.model and llm.resolve(llm, caller, state.model)
		local p = cur and providers[cur.provider]
		return p ~= nil and (p.no_key == true or not p.login)
	end

	--- auth_status(provider?) -> JSON-safe masked credential status without secrets
	function llm.auth_status(self, caller, provider_name)
		if provider_name ~= nil and (type(provider_name) ~= "string" or provider_name == "") then
			lib.fail("invalid_arguments", "provider must be a non-empty string")
		end
		local rows = {}
		local matched = false
		for _, r in ipairs(login_status(caller)) do
			if provider_name == nil or r.provider == provider_name then
				matched = true
				local overridden = {}
				for _, s in ipairs(r.overridden or {}) do
					overridden[#overridden + 1] = s
				end
				rows[#rows + 1] = {
					provider = r.provider,
					label = r.label,
					logged_in = r.logged_in == true,
					no_key = r.no_key,
					method = r.method,
					account = r.account,
					expires_at_ms = lib.to_epoch_ms(r.expires_at_ms),
					source = r.source,
					overridden = mare.json.array(overridden),
					hint = r.hint,
					state = r.state or "registered",
					reason = r.reason,
					plugin = r.plugin,
					origin = r.origin,
					wire = r.wire,
				}
			end
		end
		if provider_name ~= nil and not matched then
			lib.fail("llm.unknown_provider", "unknown provider: " .. provider_name)
		end
		return {
			providers = mare.json.array(rows),
			has_credentials = self:has_credentials(caller) == true,
		}
	end

	--- list_providers() -> {data = masked login status per provider}
	function llm.list_providers(self, caller)
		return { data = self:auth_status(caller).providers }
	end

	--- auth_keys(prefix?) -> sorted credential key names in auth.json, optionally filtered by
	--- literal prefix. Never returns credential values and never invokes key commands.
	function llm.auth_keys(_, _, prefix)
		if prefix ~= nil and type(prefix) ~= "string" then
			lib.fail("invalid_arguments", "prefix must be a string")
		end
		local proj = mare.projection
		if prefix ~= nil and proj and proj.contains and proj.contains(prefix) then
			lib.fail("invalid_arguments", "prefix must not contain a projected value")
		end
		local ok_mode, mode_err = D.auth_store.check_mode()
		if not ok_mode then
			lib.fail("io", tostring(mode_err))
		end
		local auth, err, err_kind = read_auth()
		if not auth then
			lib.fail(err_kind == "corrupt" and "corrupt" or "io", tostring(err or "cannot read auth store"))
		end
		local active_proj = proj and proj.active and proj.active()
		local function has_literal_marker(s)
			if s:find("[[mare-value:", 1, true) ~= nil then
				return true
			end
			return proj and proj.contains and proj.contains(s) or false
		end
		local keys = {}
		local seen = {}
		local seen_projected = {}
		local ambiguous_in_scope = false
		for k, v in pairs(auth) do
			if type(k) == "string" and k ~= "version" and v ~= nil then
				local raw_in_scope = prefix == nil or k:sub(1, #prefix) == prefix
				local out_key = k
				local proj_in_scope = raw_in_scope
				local ambiguous = has_literal_marker(k)
				local projected_key
				if active_proj and proj and proj.project then
					local projected, lossy = proj.project(k)
					if type(projected) == "string" and projected ~= "" then
						projected_key = projected
					end
					if
						type(projected) ~= "string"
						or projected == ""
						or lossy
						or projected ~= k
						or has_literal_marker(projected)
					then
						ambiguous = true
						if type(projected) == "string" and prefix ~= nil and projected:sub(1, #prefix) == prefix then
							proj_in_scope = true
						end
					else
						out_key = projected
						proj_in_scope = prefix == nil or out_key:sub(1, #prefix) == prefix
					end
				end
				if raw_in_scope or proj_in_scope then
					if projected_key ~= nil then
						if seen_projected[projected_key] then
							lib.fail("invalid_arguments", "projected credential key names collide")
						end
						seen_projected[projected_key] = true
					end
					if ambiguous then
						ambiguous_in_scope = true
					else
						if seen[out_key] then
							lib.fail("invalid_arguments", "projected credential key names collide")
						end
						seen[out_key] = true
						keys[#keys + 1] = out_key
					end
				end
			end
		end
		if ambiguous_in_scope then
			lib.fail("invalid_arguments", "credential key names are ambiguous in the requested scope")
		end
		table.sort(keys)
		return mare.json.array(keys)
	end

	function llm.list_auth_keys(self, caller, prefix)
		if type(prefix) == "table" then
			prefix = prefix.prefix
		end
		return { data = self:auth_keys(caller, prefix) }
	end

	return { auth_keys = auth_keys }
end
