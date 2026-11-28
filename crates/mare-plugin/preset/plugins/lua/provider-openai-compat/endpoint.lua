-- provider-openai-compat.endpoint: `openai-chat` wire `provider(name, entry)` factory.
-- Turns any data-tier provider entry (shipped preset, vendor-pack plugin, or `mare.provider`)
-- into runtime hooks (`endpoint`, `stream`, `warm_url`, `login`, `catalog`, `errors`, `params`).
-- All quirks follow explicit narrow fields -> `entry.dialect` -> host detection (never provider name).
local function host_matches(url, domain)
	if type(url) ~= "string" then
		return false
	end
	local h = (url:match("^https?://([^/:?#]+)") or ""):lower()
	return h == domain or (#h > #domain and h:sub(-(#domain + 1)) == "." .. domain)
end

local EFFORT_WIRE = {
	openrouter = {
		options = {
			{ value = "off", wire = { chat = {} } },
			{ value = "minimal", wire = { chat = { ["reasoning.effort"] = "low" } } },
			{ value = "xhigh", wire = { chat = { ["reasoning.effort"] = "high" } } },
		},
		wire = { chat = { ["reasoning.effort"] = "$value" } },
	},
	reasoning_effort = {
		options = { { value = "off", wire = { chat = {} } } },
		wire = { chat = { reasoning_effort = "$value" } },
	},
}

local DEFAULT_REASONING_BUDGETS = {
	minimal = 1024,
	low = 2048,
	medium = 8192,
	high = 16384,
	xhigh = 32768,
}

local function resolve_reasoning_format(entry, base_url)
	if entry.reasoning_format then
		return entry.reasoning_format
	end
	if entry.effort_param == "openrouter" then
		return "openrouter"
	elseif entry.effort_param == "reasoning_effort" then
		return "reasoning_effort"
	end
	local url = base_url or entry.base_url
	local d = type(entry.dialect) == "string" and entry.dialect:lower() or ""
	if d == "openrouter" or host_matches(url, "openrouter.ai") then
		return "openrouter"
	elseif d == "deepseek" or host_matches(url, "deepseek.com") then
		return "deepseek"
	elseif d == "zai" or host_matches(url, "z.ai") or host_matches(url, "bigmodel.cn") then
		return "zai"
	elseif d == "together" or host_matches(url, "together.xyz") then
		return "together"
	elseif d == "qwen" then
		return "qwen"
	elseif d == "qwen-chat-template" or d == "chat-template" then
		return "qwen-chat-template"
	end
	return nil
end

local function build_param_rules(name, entry, live_entry, live_base_url)
	local tf = resolve_reasoning_format(entry, live_base_url(entry))
	local budget_field = entry.reasoning_budget_field
	local function apply_budget(cur, req, effort_val)
		local bf = cur.reasoning_budget_field
		if type(bf) == "string" and bf ~= "" and effort_val ~= "off" then
			local budgets = cur.reasoning_budgets or DEFAULT_REASONING_BUDGETS
			req.body[bf] = budgets[effort_val] or DEFAULT_REASONING_BUDGETS.medium
		end
	end

	if tf == "openrouter" or tf == "reasoning_effort" then
		local spec = EFFORT_WIRE[tf]
		if budget_field then
			local base_wire = spec.wire.chat
			spec = {
				options = spec.options,
				wire = {
					chat = function(req, c)
						local cur = live_entry()
						for k, v in pairs(base_wire) do
							local resolved = v == "$value" and c.value or v
							if k == "reasoning.effort" then
								req.body.reasoning = req.body.reasoning or {}
								req.body.reasoning.effort = resolved
							else
								req.body[k] = resolved
							end
						end
						apply_budget(cur, req, c.value)
					end,
				},
			}
		end
		return {
			{
				provider = name,
				route = "chat",
				define = { effort = spec },
			},
		}
	elseif tf or budget_field then
		return {
			{
				provider = name,
				route = "chat",
				when = function(m)
					return m.reasoning == true
				end,
				define = {
					effort = {
						wire = {
							chat = function(req, c)
								local cur = live_entry()
								local cur_tf = resolve_reasoning_format(cur, live_base_url(cur))
								local on = c.value ~= "off"
								if cur_tf == "deepseek" then
									req.body.thinking = { type = on and "enabled" or "disabled" }
									if on and cur.supports_reasoning_effort then
										req.body.reasoning_effort = c.value
									end
								elseif cur_tf == "zai" then
									req.body.thinking = on and { type = "enabled", clear_thinking = false }
										or { type = "disabled" }
								elseif cur_tf == "qwen" then
									req.body.enable_thinking = on
								elseif cur_tf == "qwen-chat-template" or cur_tf == "chat-template" then
									req.body.chat_template_kwargs = {
										enable_thinking = on,
										preserve_thinking = true,
									}
								elseif cur_tf == "together" then
									req.body.reasoning = { enabled = on }
								end
								apply_budget(cur, req, c.value)
							end,
						},
					},
				},
			},
		}
	end
	return nil
end

return function(D)
	local llm, cfg = D.llm, D.cfg
	local ensure_wire = type(D.wire) == "function" and D.wire or function()
		return D.wire
	end

	return function(name, entry)
		local wire_stream = llm:wire("openai-chat")
		local envs = (type(entry.api_key_env) == "string" and entry.api_key_env ~= "") and { entry.api_key_env } or {}
		local cfg_has_key = type(((mare.config.providers or {})[name] or {}).api_key) ~= "nil"
		-- Config-defined credentials (`{ cmd = ... }`, uppercase ENV_NAME, or literal string)
		-- resolve through `llm:api_key`; plugin/RPC literal `api_key` stays available when config
		-- does not set one.
		local raw_key = not cfg_has_key and type(entry.api_key) == "string" and entry.api_key ~= "" and entry.api_key
			or nil

		local function live_entry()
			local eff = llm:provider_config(name)
			if type(eff) == "table" and next(eff) ~= nil then
				return eff
			end
			return entry
		end

		local function live_base_url(eff)
			return llm:base_url(name, (eff and eff.base_url) or entry.base_url)
		end

		local function api_key()
			return raw_key or llm:api_key(name, envs)
		end

		local function endpoint(_req)
			local eff = live_entry()
			local key = api_key()
			if not key and not eff.no_key then
				error({
					message = ("%s is not configured: run /login %s or set %s"):format(
						name,
						name,
						eff.api_key_env or "an API key"
					),
					retryable = false,
				})
			end
			-- plugin-wide headers first, the endpoint's own win
			local merged_headers = {}
			for k, v in pairs(cfg.headers or {}) do
				merged_headers[k] = v
			end
			for k, v in pairs(eff.headers or {}) do
				merged_headers[k] = v
			end
			local base = live_base_url(eff)
			return {
				provider = name,
				label = eff.label or name,
				dialect = eff.dialect,
				base_url = base,
				url = base .. "/chat/completions",
				auth = "bearer",
				key = key,
				headers = merged_headers,
				drop_fields = eff.drop_fields,
				transport = eff.transport or cfg.transport,
				endpoint_config = eff,
			}
		end

		local function stream(req, sink)
			return wire_stream(endpoint, req, sink)
		end

		local dialect_lc = type(entry.dialect) == "string" and entry.dialect:lower() or ""
		local use_cerebras_map = dialect_lc == "cerebras" or host_matches(entry.base_url, "cerebras.ai")

		return {
			route = "chat",
			wire = "openai-chat",
			endpoint = endpoint,
			warm_url = function()
				return live_base_url(live_entry())
			end,
			model_defaults = require("mare_lib").APPLY_PATCH_MODEL_DEFAULTS,
			-- a keyless endpoint (ollama, lmstudio) is no reason to skip the first-run guide
			no_key = entry.no_key or nil,
			login = not entry.no_key and {
				methods = { "api_key" },
				run = function(lio)
					llm:login_api_key(name, nil, lio)
				end,
			} or nil,
			login_label = "API key",
			-- Chat Completions ids: [A-Za-z0-9_-], at most 40 characters
			tool_id_policy = { max = 40 },
			is_configured = function()
				return entry.no_key or raw_key ~= nil or llm:has_api_key(name, envs)
			end,
			errors = setmetatable({}, {
				__index = function(_, k)
					local w = ensure_wire()
					if k == "codes" then
						return w.COMPAT_CODES
					elseif k == "overflow" then
						return w.OVERFLOW
					elseif k == "image_limit" then
						return w.IMAGE_LIMIT
					elseif k == "image_unsupported" then
						return w.IMAGE_UNSUPPORTED
					elseif k == "map" then
						return use_cerebras_map and w.cerebras_overflow_map or nil
					end
				end,
			}),
			stream = stream,
			params = build_param_rules(name, entry, live_entry, live_base_url),
			catalog_target = entry.list_models ~= false and function()
				local eff = live_entry()
				return {
					url = mare.http.build_url(live_base_url(eff) .. "/models"),
					headers = { cfg = cfg.headers, ep = eff.headers },
				}
			end or nil,
			catalog_scope = (entry.list_models ~= false and raw_key ~= nil) and function()
				return "api_key:" .. mare.crypto.sha256(raw_key)
			end or nil,
			-- model catalogue: declarative GET {base}/models fetched via llm catalog.fetch
			catalog = entry.list_models ~= false and function(for_fetch)
				local eff = live_entry()
				local headers = {}
				for k, v in pairs(cfg.headers or {}) do
					headers[k] = v
				end
				for k, v in pairs(eff.headers or {}) do
					headers[k] = v
				end
				return {
					label = ("%s GET /models"):format(name),
					url = live_base_url(eff) .. "/models",
					headers = headers,
					auth = "bearer",
					key = for_fetch and api_key() or nil,
					no_key = eff.no_key,
					decode = function(raw)
						local items = require("mare_lib").decode_models_envelope(
							raw,
							"data",
							"id",
							("%s GET /models"):format(name)
						)
						local wire = ensure_wire()
						local out = {}
						for _, m in ipairs(items) do
							out[#out + 1] = wire.listed_model(m)
						end
						return out
					end,
				}
			end or nil,
		}
	end
end
