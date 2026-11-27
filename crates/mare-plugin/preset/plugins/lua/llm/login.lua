-- llm.login: login / logout flows, login I/O and the `mare login` CLI (login/cli);
-- part of the `llm` plugin (plugins/llm.lua), loaded at the start of its apply.
local lib = require("mare_lib")

return function(D)
	local ctx = D.ctx
	local providers, llm = D.providers, D.llm
	local read_auth = D.auth_store.read_auth
	local key_cache = D.api_keys.key_cache
	local known_envs = D.api_keys.known_envs
	local auth_keys = D.credentials.auth_keys
	local all_providers = D.all_providers

	--- headless browsers can't open a login page: SSH sessions and Linux without a display
	--- (device-code login is offered first there)
	function llm.no_browser(_, _)
		if (mare.env("SSH_CONNECTION") or mare.env("SSH_TTY") or "") ~= "" then
			return true
		end
		return mare.os == "linux" and (mare.env("DISPLAY") or "") == "" and (mare.env("WAYLAND_DISPLAY") or "") == ""
	end

	local function login_spec(p)
		if not p or not p.login then
			return nil, nil
		end
		if type(p.login) == "table" then
			return p.login.run, p.login.methods
		end
		if type(p.login) == "function" then
			if p.oauth_key then
				return p.login, { "oauth", "device", "api_key" }
			end
			return p.login, { "api_key" }
		end
		return nil, nil
	end

	local function has_method(methods, target)
		if type(methods) ~= "table" then
			return false
		end
		for _, m in ipairs(methods) do
			if m == target then
				return true
			end
		end
		return false
	end

	local function login_capable_names()
		local n = {}
		for _, x in ipairs(all_providers()) do
			if login_spec(x) then
				n[#n + 1] = x.name
			end
		end
		return n
	end

	--- login_io(io?) -> the I/O a provider's login flow uses, filled with the TUI defaults:
	--- say(text), ask(title, {secret?, placeholder?}) -> string|nil, choose(title, items) ->
	--- index|nil; `key` (an API key given up front: `mare login --with-api-key`), `method`
	--- ("oauth" | "device" | "api_key"). `mare login` passes its own say / ask.
	function llm.login_io(_, _, lio)
		lio = lio or {}
		local out = {}
		for k, v in pairs(lio) do
			out[k] = v
		end
		if type(out.cancel) == "function" then
			out.cancel = out.cancel()
		end
		out.say = out.say
			or function(text)
				if mare.mode == "rpc" and mare.ui.connected() then
					mare.ui.notify(tostring(text), "info")
					return
				end
				local lines = {}
				for l in (tostring(text) .. "\n"):gmatch("(.-)\n") do
					lines[#lines + 1] = mare.ui.Line({ mare.ui.Span(l) })
				end
				mare.ui.print(mare.ui.Lines(lines))
			end
		out.ask = out.ask
			or function(title, o)
				o = o or {}
				return mare.ui.input({ title = title, secret = o.secret, placeholder = o.placeholder })
			end
		out.choose = out.choose
			or (lio.ask and function(title, items)
				local lines = { title .. ":" }
				for i, it in ipairs(items) do
					lines[#lines + 1] = ("  %d. %s%s"):format(i, it.label, it.detail and ("  — " .. it.detail) or "")
				end
				out.say(table.concat(lines, "\n"))
				return tonumber(out.ask("Number", {}) or "")
			end)
			or function(title, items)
				return mare.ui.select({ title = title, items = items })
			end
		return out
	end
	--- Identify a registered login-capable provider from a pasted API key using declared
	--- `key_prefixes`. Returns the provider name only for a single unambiguous longest-prefix
	--- match; returns nil when unmatched or ambiguous.
	function llm.identify_key_provider(_, _, key)
		if type(key) ~= "string" then
			return nil
		end
		local trimmed = key:match("^%s*(.-)%s*$")
		if not trimmed or trimmed == "" then
			return nil
		end
		local best_len = 0
		local matches = {}
		for _, p in ipairs(all_providers()) do
			if login_spec(p) and type(p.key_prefixes) == "table" then
				local p_best = 0
				for _, pfx in ipairs(p.key_prefixes) do
					if type(pfx) == "string" and pfx ~= "" and trimmed:sub(1, #pfx) == pfx and #pfx > p_best then
						p_best = #pfx
					end
				end
				if p_best > 0 then
					if p_best > best_len then
						best_len = p_best
						matches = { p.name }
					elseif p_best == best_len then
						matches[#matches + 1] = p.name
					end
				end
			end
		end
		if #matches == 1 then
			return matches[1]
		end
		return nil
	end

	--- the API-key half of every provider's login: masked prompt (or `io.key`), stored 0600
	function llm.login_api_key(_, caller, name, title, lio)
		lio = llm.login_io(llm, caller, lio)
		if not lio.key then
			local p = providers[name]
			if p and type(p.key_prefixes) == "table" and #p.key_prefixes > 0 then
				lio.say(("API key for %s (expected prefix: %s)."):format(name, table.concat(p.key_prefixes, ", ")))
			else
				lio.say(
					("API key for %s: check the provider's documented prefix; shared prefixes such as sk- do not identify a provider."):format(
						name
					)
				)
			end
		end
		local key = lio.key or lio.ask(title or (name .. " API key"), { secret = true })
		key = key and key:match("^%s*(.-)%s*$")
		if not key or key == "" then
			lib.fail("llm.login_failed", "no API key given")
		end
		local detected = llm.identify_key_provider(llm, caller, key)
		if detected and detected ~= name then
			lio.say(("Note: this key's prefix matches %s, not %s."):format(detected, name))
		end
		local ok, err = pcall(function()
			llm.auth_set(llm, caller, name, { type = "api_key", key = key })
			llm.invalidate_api_key(llm, caller, name)
		end)
		if not ok then
			local message, out, at = tostring(err), {}, 1
			while true do
				local first, last = message:find(key, at, true)
				if not first then
					out[#out + 1] = message:sub(at)
					break
				end
				out[#out + 1] = message:sub(at, first - 1)
				out[#out + 1] = "[redacted]"
				at = last + 1
			end
			error(table.concat(out), 0)
		end
	end

	--- login(provider, io?) — runs the provider's login, then refreshes its model list in the
	--- background
	function llm.login(_, caller, name, lio)
		local p = providers[name]
		local run, methods = login_spec(p)
		if not run then
			local st = not p and D.directory and D.directory.status and D.directory.status(name) or nil
			if st and st.state ~= "registered" then
				lib.fail("llm.unknown_provider", st.reason or ("%s is %s"):format(tostring(name), st.state))
			end
			lib.fail(
				"llm.unknown_provider",
				("%s has no login (known: %s)"):format(tostring(name), table.concat(login_capable_names(), ", "))
			)
		end
		local io_arg = llm.login_io(llm, caller, lio)
		local requested_method = io_arg.method or (io_arg.key and "api_key")
		if requested_method == "auto" and type(p.login) == "table" then
			local resolved
			if llm.no_browser(llm, caller) and has_method(methods, "device") then
				resolved = "device"
			elseif has_method(methods, "oauth") then
				resolved = "oauth"
			elseif has_method(methods, "device") then
				resolved = "device"
			else
				resolved = methods and methods[1] or nil
			end
			if not resolved then
				lib.fail("llm.login_failed", ("%s has no usable login method"):format(name))
			end
			io_arg.method = resolved
			requested_method = resolved
		end
		if requested_method and requested_method ~= "auto" and not has_method(methods, requested_method) then
			lib.fail(
				"llm.login_failed",
				("%s does not support login method %q (supported: %s)"):format(
					name,
					tostring(requested_method),
					table.concat(methods or {}, ", ")
				)
			)
		end
		if requested_method and io_arg.method == nil then
			io_arg.method = requested_method
		end
		-- an interactive flow needs someone to answer: a frontend or an explicit key
		if not (lio and (lio.key or lio.ask or lio.choose)) and mare.mode == "rpc" and not mare.ui.connected() then
			lib.fail("llm.needs_ui", "interactive login requires a connected UI or an explicit key")
		end
		-- a provider's failure is unexpected unless it raises llm.login_failed itself
		local res = run(io_arg)
		if io_arg.cancel and io_arg.cancel:is_cancelled() then
			lib.fail("cancelled", "login cancelled")
		end
		if res == false then
			lib.fail("llm.login_failed", "login cancelled")
		end
		key_cache[name] = nil
		ctx.bus:emit("llm/providers-changed")
		ctx.bus:emit("llm/login", { provider = name })
		if p.catalog or p.list_models then
			ctx:spawn(function()
				llm.refresh_catalog(llm, caller, { name = name, force = true })
			end)
		end
	end

	--- logout(provider) -> removed auth.json keys, kept sources: every stored credential of the
	--- provider goes (`openai` = the API key and the ChatGPT login), each token revoked first
	--- where the provider can (best effort:55). Environment
	--- variables and init.lua keys can't be removed here: they're returned so callers say so.
	function llm.logout(_, caller, name)
		local p = providers[name]
		local guard = mare.lock.file(mare.paths.auth .. ".lock", { timeout_ms = 30000 })
		local ok, res = pcall(function()
			local t, rerr = read_auth()
			if not t then
				error(rerr, 0)
			end
			local removed, warnings = {}, {}
			for _, key in ipairs(auth_keys(name)) do
				local v = t[key]
				if v ~= nil then
					if p and p.revoke and type(v) == "table" then
						local okr, rerr2 = pcall(p.revoke, key, v)
						if not okr then
							warnings[#warnings + 1] = ("could not revoke %s: %s"):format(key, tostring(rerr2))
							mare.log("warn", warnings[#warnings])
						end
					end
					llm.auth_set(llm, caller, key, nil, { locked = true })
					removed[#removed + 1] = key
				end
			end
			return { removed = removed, warnings = warnings }
		end)
		guard:release()
		if not ok then
			error(res, 0)
		end
		key_cache[name] = nil
		res.kept = {}
		for _, s in ipairs(llm.credential_sources(llm, caller, name)) do
			res.kept[#res.kept + 1] = s.source
		end
		ctx.bus:emit("llm/providers-changed")
		ctx.bus:emit("llm/logout", { provider = name, removed = res.removed })
		return {
			removed = mare.json.array(res.removed),
			warnings = mare.json.array(res.warnings),
			kept = mare.json.array(res.kept),
		}
	end
	--- providers with something stored in auth.json (the /logout picker)
	function llm.logged_in(_, caller)
		local t, err = read_auth()
		if not t then
			error(err, 0)
		end
		local out = {}
		for _, p in ipairs(all_providers()) do
			for _, key in ipairs(auth_keys(p.name)) do
				if t[key] ~= nil then
					out[#out + 1] = p.name
					break
				end
			end
		end
		return out
	end

	-- `mare login [provider] [--with-api-key] [--device-auth]`, `mare login status`,
	-- `mare logout [provider]`, `mare model refresh`:
	-- the CLI emits `login/cli` with its own say / ask and reads the exit code back
	local function on_cli(req)
		local lio = llm.login_io(llm, ctx, { say = req.say, ask = req.ask, key = req.key, method = req.method })
		local function fail(msg, code)
			lio.say("mare: " .. msg)
			req.code = code or 1
		end
		if req.action == "status" then
			for _, l in ipairs(llm.status_lines(llm, ctx)) do
				lio.say(l.text)
			end
			return
		elseif req.action == "models-refresh" then
			local res = llm.refresh_catalog(llm, ctx, { force = true, configured = true })
			local names = {}
			for n in pairs(res) do
				names[#names + 1] = n
			end
			table.sort(names)
			if #names == 0 then
				lio.say("no provider with credentials lists models — log in first (`mare login <provider>`)")
			end
			for _, n in ipairs(names) do
				local v = res[n]
				if type(v) == "number" then
					lio.say(("%s: refreshed (%d new)"):format(n, v))
				else
					fail(("%s: %s"):format(n, tostring(v)))
				end
			end
			return
		elseif req.action == "logout" then
			local name = req.provider
			if not name or name == "" then
				local list = llm.logged_in(llm, ctx)
				if #list == 0 then
					lio.say("nothing stored in " .. mare.paths.auth .. " — not logged in")
					return
				elseif #list == 1 then
					name = list[1]
				else
					return fail(
						"logged in to " .. table.concat(list, ", ") .. " — name one: mare logout <provider>",
						2
					)
				end
			end
			local ok, res = pcall(llm.logout, llm, ctx, name)
			if not ok then
				return fail(tostring(res))
			end
			for _, w in ipairs(res.warnings) do
				lio.say("warning: " .. w)
			end
			if #res.removed == 0 then
				lio.say(("%s: nothing stored to remove"):format(name))
			else
				lio.say(("logged out of %s (removed %s)"):format(name, table.concat(res.removed, ", ")))
			end
			if #res.kept > 0 then
				lio.say(("%s is still configured by %s — remove it there"):format(name, table.concat(res.kept, ", ")))
			end
			return
		end
		if req.key ~= nil and req.key:match("^%s*$") then
			return fail("--with-api-key: no key on stdin", 2)
		end
		-- login: explicit provider name, else configured default model's provider,
		-- else interactive selector on TTY or exit 2 listing login-capable providers
		local name = req.provider
		if name == nil or name == "" then
			local default_ref = D.state and (D.state.default_model or D.state.model)
			if type(default_ref) == "string" and default_ref ~= "" then
				local m = llm.resolve(llm, ctx, default_ref)
				if m and m.provider and login_spec(providers[m.provider]) then
					name = m.provider
				end
			end
		end
		local login_list = {}
		for _, prov in ipairs(all_providers()) do
			if login_spec(prov) then
				login_list[#login_list + 1] = prov
			end
		end
		if name == nil or name == "" then
			local names = login_capable_names()
			if #login_list == 0 then
				return fail("no login-capable providers are registered", 2)
			end
			if req.tty then
				local items = {}
				for i, prov in ipairs(login_list) do
					items[i] = { label = prov.name, detail = prov.login_label }
				end
				local idx = lio.choose("Log in to", items)
				local chosen = idx and login_list[idx]
				if not chosen then
					return fail("login cancelled", 1)
				end
				name = chosen.name
			else
				return fail(
					("specify a provider: mare login <provider> (login-capable: %s)"):format(table.concat(names, ", ")),
					2
				)
			end
		end
		local p = providers[name]
		local run, methods = login_spec(p)
		if not run then
			return fail(
				("%s has no login (known: %s)"):format(tostring(name), table.concat(login_capable_names(), ", ")),
				2
			)
		end
		local requested_method = req.method or ((req.key ~= nil or req.with_api_key_tty) and "api_key" or nil)
		if requested_method and not has_method(methods, requested_method) then
			return fail(
				("provider %s does not support login method %q (supported: %s)"):format(
					name,
					tostring(requested_method),
					table.concat(methods or {}, ", ")
				),
				2
			)
		end
		if req.with_api_key_tty then
			if p.is_configured and not known_envs[name] then
				pcall(p.is_configured)
			end
			local env_name = (known_envs[name] and known_envs[name][1]) or "API_KEY"
			req.tty_hint = ("mare: --with-api-key reads the key from stdin: printenv %s | mare login %s --with-api-key"):format(
				env_name,
				name
			)
			req.code = 2
			return
		end
		local supports_oauth = has_method(methods, "oauth") or has_method(methods, "device")
		if not req.key and not supports_oauth then
			-- API-key providers: the key comes from stdin, never typed visibly on a terminal
			return fail(("pass the key on stdin: printenv KEY | mare login %s --with-api-key"):format(name), 2)
		end
		if not lio.method and supports_oauth and not req.key then
			-- OAuth-capable providers default CLI login without --with-api-key/--device-auth/--oauth to auto
			lio.method = "auto"
		end
		local ok, err = pcall(llm.login, llm, ctx, name, lio)
		if not ok then
			return fail(
				"login failed: " .. tostring(type(err) == "table" and (err.message or mare.json.encode(err)) or err)
			)
		end
		lio.say("logged in to " .. name)
	end
	if D.lazy_login then
		return { on_cli = on_cli }
	end
	ctx:on("login/cli", on_cli)
end
