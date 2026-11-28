-- llm: is the selected model ready to be sent to?. Loaded on the first
-- prompt. Runs in the llm plugin's env.
--
-- The llm part of the readiness contract, checked in this order, the first failure refusing the
-- prompt as "not sent: <reason>" before anything is persisted or sent:
--   1. a model is selected;
--   2. its provider plugin hasn't failed;
--   3. its provider is registered;
--   4. the provider has credentials,
--      and can serve this model (`unavailable_for(model)`: e.g. the wire plugin of the model's
--      route is disabled or failed — only the models on that route are refused);
--   5. the provider's `prepare` (run in request/prepare: e.g. refreshing a ChatGPT token that is
--      about to expire) didn't refuse; a temporary
--      failure is retried a little first (llm.prepare).
-- Only the active `llm` service judges: a replacement llm owns its own readiness.
local M = {}

--- The reason no model is selected, worded for this frontend (headless: `mare login`, not /login).
function M.no_model()
	return mare.mode == "tui" and "no model configured — use /login and /model"
		or "no model configured — run `mare login <provider>` (or set its API key variable) and pass --model"
end

--- The provider's missing credentials, or nil: a provider with a login whose key isn't configured
--- (a key command, `api_key = { cmd = ... }`, is always tried).
function M.missing_credentials(p, pcfg, how_to_login)
	if p.login and not p.no_key and type(pcfg.api_key) ~= "table" and p.is_configured and not p.is_configured() then
		return ("%s is not configured: %s"):format(p.label or p.name, how_to_login(p.name))
	end
	return nil
end

local function provider_cfg(name)
	return require("llm.directory").effective_config(name)
end

--- M.new(env) -> {prepare = fn(event), ready = fn(req) -> vote | nil}
--- env: {active = fn() -> bool, current = fn() -> model | nil, provider = fn(name) -> p | nil,
---       provider_failure = fn(name) -> reason | nil, how_to_login = fn(name) -> string}
function M.new(env)
	-- per-model provider preparation failures: ref -> reason
	local refused = {}
	local R = {}

	function R.prepare(event)
		if not env.active() then
			return
		end
		local cur = env.current()
		local m = (type(event) == "table" and event.model) or cur
		local sel = env.selected_model and env.selected_model() or nil
		local is_primary = type(event) ~= "table"
			or event.model == nil
			or event.model == cur
			or (cur ~= nil and event.model.ref == cur.ref)
		if is_primary and env.ensure_loaded and type(sel) == "string" and sel ~= "" then
			local prov = string.match(sel, "^([^/]+)/")
			if env.ensure_loaded(prov and env.provider(prov) and prov or nil) then
				m = env.current()
				if type(event) == "table" then
					event.model = m
				end
			end
		end
		if not m or not m.ref then
			return
		end
		-- Request barrier: await disk-cached catalog capabilities for the selected
		-- provider before preparation or the first request runs (essential for headless/-p/RPC).
		if env.ensure_loaded and m.provider then
			if env.ensure_loaded(m.provider) then
				local hydrated = env.resolve and env.resolve(m.ref) or nil
				if hydrated then
					m = hydrated
					if type(event) == "table" and event.model ~= nil then
						event.model = hydrated
					end
				end
			end
		end
		if m.unlisted and env.warn_unlisted then
			env.warn_unlisted(m)
		end
		refused[m.ref] = nil
		local p = not m.unavailable and env.provider(m.provider)
		if not p or not p.prepare or M.missing_credentials(p, provider_cfg(m.provider), env.how_to_login) then
			return
		end
		local why = require("llm.prepare").run(p, m, type(event) == "table" and event.cancel or nil, env.how_to_login)
		if why then
			refused[m.ref] = why
		end
	end

	function R.ready(req)
		if not env.active() then
			return nil
		end
		local m = (type(req) == "table" and req.model) or env.current()
		if not m then
			local unresolved = env.unresolved_model and env.unresolved_model() or nil
			return { decision = "deny", reason = unresolved or M.no_model() }
		end
		local p = env.provider(m.provider)
		local why = m.unavailable
			or env.provider_failure(m.provider)
			or (not p and ("provider %s is not available"):format(tostring(m.provider)))
			or M.missing_credentials(p, provider_cfg(m.provider), env.how_to_login)
			or (p.unavailable_for and p.unavailable_for(m))
			or (m.ref and refused[m.ref])
		if why then
			return { decision = "deny", reason = ("model %s: %s"):format(m.ref, why) }
		end
		return nil
	end

	return R
end

return M
