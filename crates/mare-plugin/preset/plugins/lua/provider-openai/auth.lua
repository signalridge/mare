-- provider-openai.auth: credentials (ChatGPT OAuth token, API key) and the token refresh;
-- part of the `provider-openai` plugin (plugins/provider-openai.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local CLIENT_ID, AUTH_BASE = D.CLIENT_ID, D.AUTH_BASE

	local JWT_CLAIM = "https://api.openai.com/auth"
	local function decode_jwt(token)
		local payload = string.match(token or "", "^[^.]+%.([^.]+)")
		local raw = payload and mare.crypto.base64url_decode(payload)
		return raw and mare.json.decode(raw) or nil
	end

	local llm = ctx.llm

	local function oauth()
		return llm:auth_get("openai-codex")
	end
	-- whether a ChatGPT login exists, without reading it (status and readiness checks)
	local function has_oauth()
		return llm:has_auth("openai-codex")
	end
	local function api_key()
		return llm:api_key("openai", { "OPENAI_API_KEY" })
	end
	local function has_key()
		return llm:has_api_key("openai", { "OPENAI_API_KEY" })
	end

	-- refresh failures the user must act on
	local REFRESH_FAILED = {
		refresh_token_expired = "Your access token could not be refreshed because your refresh token has expired. Please log out and sign in again.",
		refresh_token_reused = "Your access token could not be refreshed because your refresh token was already used. Please log out and sign in again.",
		refresh_token_invalidated = "Your access token could not be refreshed because your refresh token was revoked. Please log out and sign in again.",
		invalid_grant = "Your access token could not be refreshed because your refresh token was revoked. Please log out and sign in again.",
		revoked = "Your access token could not be refreshed because your refresh token was revoked. Please log out and sign in again.",
	}
	local function auth_http_opts(req_opts)
		local pc = llm:provider_config("openai") or {}
		local opts = type(req_opts) == "table" and req_opts or {}
		return {
			proxy = opts.proxy ~= nil and opts.proxy or pc.proxy,
			connect_timeout_ms = opts.connect_timeout_ms ~= nil and opts.connect_timeout_ms or pc.connect_timeout_ms,
		}
	end

	local function token_request(form, timeout_ms, req_opts)
		local opts = type(req_opts) == "table" and req_opts or {}
		local http_opts = auth_http_opts(opts)
		local r = mare.http.request({
			method = "POST",
			url = (cfg.auth_base_url or AUTH_BASE) .. "/oauth/token",
			form = form,
			cancel = opts.cancel,
			timeout_ms = timeout_ms or 30000,
			proxy = http_opts.proxy,
			connect_timeout_ms = http_opts.connect_timeout_ms,
			redirect = "same-origin",
		})
		local text = r:text()
		local body = mare.json.decode(text)
		if not r.ok or type(body) ~= "table" or not body.access_token then
			local code = type(body) == "table"
				and (type(body.error) == "table" and body.error.code or body.error or body.code)
			local normalized = code and string.lower(tostring(code))
			local known = normalized and REFRESH_FAILED[normalized]
			-- only a 401 or a known refresh-token code is permanent (log in again); 408, 429,
			-- 5xx and other failures are transient: reported, retried by the next request
			--
			if form.grant_type == "refresh_token" and (r.status == 401 or known) then
				error({
					message = known or "Your access token could not be refreshed. Please log out and sign in again.",
					kind = "auth",
					revoked = (
						normalized == "refresh_token_invalidated"
						or normalized == "invalid_grant"
						or normalized == "revoked"
					) or nil,
				})
			elseif form.grant_type == "refresh_token" then
				local kind = (r.status == 429 and "rate_limit")
					or ((r.status == 408 or r.status >= 500) and "server")
					or "transport"
				error({
					message = ("token refresh failed (HTTP %d, temporary — will retry): %s"):format(
						r.status,
						text:sub(1, 300)
					),
					kind = kind,
					status = r.status,
					retry_after_ms = mare.http.retry_after(r.headers or {}),
				})
			end
			error("token request failed (" .. r.status .. "): " .. text:sub(1, 300))
		end
		local claims = decode_jwt(body.access_token) or {}
		local acct = claims[JWT_CLAIM] and claims[JWT_CLAIM].chatgpt_account_id
		-- the account's e-mail, shown masked by /status and `mare login status`
		local idc = body.id_token and decode_jwt(body.id_token) or {}
		local profile = claims["https://api.openai.com/profile"]
		local email = idc.email or (type(profile) == "table" and profile.email) or nil
		return {
			type = "oauth",
			access = body.access_token,
			refresh = body.refresh_token,
			expires = mare.task.now() + (body.expires_in or 3600) * 1000,
			account_id = acct,
			email = type(email) == "string" and email or nil,
		}
	end

	--- A valid ChatGPT token, refreshed 5 minutes early. The shared credential owner keeps the
	--- cross-process lock and persists a dispatched exchange even if its waiter is cancelled.
	local EARLY_MS = 5 * 60 * 1000
	local function fresh_oauth(force, cancel, req_opts)
		local cred, err = llm:refresh_credential("openai", "openai-codex", function(current)
			local ok, result = pcall(
				token_request,
				{ grant_type = "refresh_token", refresh_token = current.refresh, client_id = CLIENT_ID },
				15000,
				req_opts
			)
			if not ok then
				if type(result) == "table" and result.revoked then
					result.revoked = nil
					if llm:auth_set("openai-codex", nil, { locked = true, expected = current }) then
						mare.log("warn", result.message)
						if mare.ui.connected() then
							mare.ui.notify(result.message, "warning")
						end
					end
				end
				error(result, 0)
			end
			result.refresh = result.refresh or current.refresh
			result.account_id = result.account_id or current.account_id
			return result
		end, {
			force = force,
			cancel = cancel,
			valid = function(t)
				return t.expires and t.expires - EARLY_MS > mare.task.now()
			end,
			same_account = function(a, b)
				return not (a.account_id and b.account_id and b.account_id ~= a.account_id)
			end,
			refreshed = function(a, b)
				return a.access ~= b.access
			end,
		})
		if err ~= nil then
			error(err, 0)
		end
		return cred
	end

	return {
		llm = llm,
		oauth = oauth,
		has_oauth = has_oauth,
		api_key = api_key,
		has_key = has_key,
		token_request = token_request,
		fresh_oauth = fresh_oauth,
	}
end
