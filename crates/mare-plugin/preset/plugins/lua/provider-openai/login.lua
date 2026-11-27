-- provider-openai.login: ChatGPT browser / device-code login, API-key login, logout revoke;
-- part of the `provider-openai` plugin (plugins/provider-openai.lua), loaded at the start of its apply.
return function(D)
	local cfg = D.cfg
	local CLIENT_ID, AUTH_BASE = D.CLIENT_ID, D.AUTH_BASE
	local llm, token_request = D.auth.llm, D.auth.token_request
	local lib = require("mare_lib")

	--- Browser login (PKCE): the callback server takes 1455, or 1457 when 1455 is busy (the two
	--- registered redirect URIs); pasting the redirect URL always works too (SSH, another
	--- machine's browser).
	local function login_browser(lio)
		local pk = mare.crypto.pkce()
		local state = mare.crypto.random_hex(16)
		local base = cfg.auth_base_url or AUTH_BASE
		local bound_tx, bound_rx = mare.task.channel()
		local function wait_callback()
			return mare.http.listen_once({
				ports = cfg.callback_ports or { 1455, 1457 },
				path = "/auth/callback",
				timeout_ms = 600000,
				on_bound = function(port)
					bound_tx:send(port)
				end,
			})
		end
		local redirect
		local idx, a, b = mare.task.select(wait_callback, function()
			local port = bound_rx:recv()
			redirect = ("http://localhost:%d/auth/callback"):format(port)
			local url = mare.http.build_url(base .. "/oauth/authorize", {
				response_type = "code",
				client_id = CLIENT_ID,
				redirect_uri = redirect,
				scope = "openid profile email offline_access",
				code_challenge = pk.challenge,
				code_challenge_method = "S256",
				state = state,
				id_token_add_organizations = "true",
				codex_cli_simplified_flow = "true",
				originator = cfg.originator,
			})
			lio.say(
				"Opening the browser for ChatGPT login. If it doesn't open, visit:\n"
					.. url
					.. "\n\nOn another machine (SSH)? Sign in there, then paste the address the browser ends up on"
					.. " (http://localhost:"
					.. port
					.. "/auth/callback?code=…)."
			)
			if not lio.no_open then
				pcall(mare.proc.open, url)
			end
			return lio.ask("Paste the redirect URL (or finish in the browser)", {
				placeholder = redirect .. "?code=...",
			})
		end)
		local code
		if idx == 1 then
			if not a then
				error(b or "callback failed", 0)
			end
			if a.query.state ~= state then
				require("mare_lib").fail("llm.login_failed", "the pasted URL belongs to another login (state mismatch)")
			end
			code = a.query.code
			redirect = redirect or ("http://localhost:%d/auth/callback"):format(a.port or 1455)
		else
			local s2 = a or ""
			-- a pasted redirect URL must carry our state; a bare code is accepted
			local pasted_state = string.match(s2, "[?&]state=([^&%s]+)")
			if pasted_state and pasted_state ~= state then
				require("mare_lib").fail("llm.login_failed", "the pasted URL belongs to another login (state mismatch)")
			end
			code = string.match(s2, "code=([^&%s]+)") or s2
		end
		if not code or code == "" then
			require("mare_lib").fail("llm.login_failed", "no authorization code was given")
		end
		local tok = token_request({
			grant_type = "authorization_code",
			client_id = CLIENT_ID,
			code = code,
			code_verifier = pk.verifier,
			redirect_uri = redirect,
		})
		if not tok.account_id then
			error("failed to read ChatGPT account id from token", 0)
		end
		llm:auth_set("openai-codex", tok)
	end

	local function check_not_cancelled(lio)
		if lio.cancel and lio.cancel:is_cancelled() then
			lib.fail("cancelled", "login cancelled")
		end
	end

	--- Device-code login (SSH, no display): show a URL and a one-time code, poll until the
	--- user approves it in any browser (the `/api/accounts/deviceauth` endpoints, 403/404 = still
	--- pending, 15 minutes, then the code is exchanged with the redirect URI
	--- `{issuer}/deviceauth/callback`).
	local function login_device(lio)
		local function device_fallback_err(msg)
			return { message = msg, _device_fallback = true }
		end
		local function fallback_request(opts)
			check_not_cancelled(lio)
			local ok, res = pcall(mare.http.request, opts)
			check_not_cancelled(lio)
			if not ok then
				error(device_fallback_err(tostring(res)), 0)
			end
			return res
		end
		check_not_cancelled(lio)
		local base = (cfg.auth_base_url or AUTH_BASE):gsub("/+$", "")
		local api = base .. "/api/accounts"
		local r = fallback_request({
			method = "POST",
			url = api .. "/deviceauth/usercode",
			json = { client_id = CLIENT_ID },
			cancel = lio.cancel,
			timeout_ms = 30000,
			redirect = "same-origin",
		})
		if r.status == 404 then
			error(device_fallback_err("device code login is not enabled for this server — use the browser login"), 0)
		elseif not r.ok then
			error(device_fallback_err(("device code request failed (HTTP %d)"):format(r.status)), 0)
		end
		local uc = r:json() or {}
		local user_code = uc.user_code or uc.usercode
		if type(uc.device_auth_id) ~= "string" or type(user_code) ~= "string" then
			error(device_fallback_err("device code request: unexpected response"), 0)
		end
		local interval = math.max(1, tonumber(uc.interval) or 5)
		lio.say(
			"Sign in with ChatGPT using a device code:\n"
				.. "  1. Open "
				.. base
				.. "/codex/device in a browser and sign in\n"
				.. "  2. Enter this one-time code (expires in 15 minutes): "
				.. user_code
				.. "\nContinue only if you started this login in mare. If a website or another person gave you this code, cancel."
		)
		local deadline = mare.task.clock() + (cfg.device_code_timeout_ms or 15 * 60 * 1000)
		local got
		while true do
			local t = fallback_request({
				method = "POST",
				url = api .. "/deviceauth/token",
				json = { device_auth_id = uc.device_auth_id, user_code = user_code },
				cancel = lio.cancel,
				timeout_ms = 30000,
				redirect = "same-origin",
			})
			if t.ok then
				got = t:json() or {}
				break
			end
			if t.status ~= 403 and t.status ~= 404 then
				error(device_fallback_err(("device code login failed (HTTP %d)"):format(t.status)), 0)
			end
			if mare.task.clock() >= deadline then
				error(device_fallback_err("device code login timed out after 15 minutes"), 0)
			end
			if not mare.task.sleep(math.min(interval * 1000, deadline - mare.task.clock()), lio.cancel) then
				lib.fail("cancelled", "login cancelled")
			end
		end
		check_not_cancelled(lio)
		if type(got.authorization_code) ~= "string" or type(got.code_verifier) ~= "string" then
			error("device code login: unexpected token response", 0)
		end
		local tok_ok, tok = pcall(token_request, {
			grant_type = "authorization_code",
			client_id = CLIENT_ID,
			code = got.authorization_code,
			code_verifier = got.code_verifier,
			redirect_uri = base .. "/deviceauth/callback",
		}, nil, { cancel = lio.cancel })
		check_not_cancelled(lio)
		if not tok_ok then
			error(device_fallback_err(tostring(type(tok) == "table" and tok.message or tok)), 0)
		end
		if not tok.account_id then
			error("failed to read ChatGPT account id from token", 0)
		end
		llm:auth_set("openai-codex", tok, { cancel = lio.cancel })
	end

	--- login(io): ChatGPT (browser OAuth or device code) or an API key. Without a browser (SSH, no
	--- display) the device code comes first; if it fails, the browser login is offered.
	local function login(lio)
		lio = llm:login_io(lio)
		local method = lio.method or (lio.key and "api_key")
		if not method then
			local headless = llm:no_browser()
			local items = {
				{ label = "ChatGPT subscription", detail = "browser sign-in", method = "oauth" },
				{
					label = "ChatGPT subscription (device code)",
					detail = "sign in on any device",
					method = "device",
				},
				{ label = "API key", detail = "platform.openai.com", method = "api_key" },
			}
			if headless then
				items[1], items[2] = items[2], items[1]
			end
			local i = lio.choose("OpenAI login", items)
			method = i and items[i] and items[i].method
			if not method then
				lib.fail("llm.login_failed", "login cancelled")
			end
		end
		if method == "auto" then
			-- `mare login`: device code where no browser can open (SSH, no display)
			method = llm:no_browser() and "device" or "oauth"
		end
		if method == "device" then
			local ok, err = pcall(login_device, lio)
			if not ok then
				local cancelled = (lio.cancel and lio.cancel:is_cancelled())
					or (type(err) == "table" and (err.code == "cancelled" or err.code == "aborted"))
				if not cancelled and type(err) == "table" and err._device_fallback then
					lio.say(
						"Device code login failed ("
							.. tostring(err.message or err)
							.. ") — falling back to the browser login."
					)
					login_browser(lio)
				else
					if type(err) == "table" and err.code ~= nil then
						error(err, 0)
					end
					lib.fail(
						cancelled and "cancelled" or "llm.login_failed",
						tostring(type(err) == "table" and err.message or err)
					)
				end
			end
		elseif method == "oauth" then
			login_browser(lio)
		elseif method == "api_key" then
			llm:login_api_key("openai", "OpenAI API key", lio)
		else
			lib.fail("llm.login_failed", ("unsupported login method: %s"):format(tostring(method)))
		end
	end

	--- logout: revoke the ChatGPT refresh token (or the access token) before it is deleted;
	--- a failure doesn't keep the login
	local function revoke(key, entry)
		if key ~= "openai-codex" then
			return
		end
		local tok, hint = entry.refresh, "refresh_token"
		if not tok or tok == "" then
			tok, hint = entry.access, "access_token"
		end
		if not tok or tok == "" then
			return
		end
		local r = mare.http.request({
			method = "POST",
			url = (cfg.auth_base_url or AUTH_BASE) .. "/oauth/revoke",
			json = { token = tok, token_type_hint = hint, client_id = hint == "refresh_token" and CLIENT_ID or nil },
			timeout_ms = 10000,
			redirect = "same-origin",
		})
		if not r.ok then
			error(("revoking the %s failed (HTTP %d)"):format(hint, r.status), 0)
		end
	end

	return {
		spec = {
			methods = D.LOGIN_METHODS,
			run = login,
		},
		login = login,
		revoke = revoke,
	}
end
