-- provider-opencode-go.provider: the `opencode-go` provider — catalogue, credentials, the route
-- of each model (llm/routes), each route's endpoint and the hand-off to that route's wire. Part
-- of the `provider-opencode-go` plugin (plugins/provider-opencode-go.lua), loaded in its apply.
return function(D)
	local ctx, cfg, llm = D.ctx, D.cfg, D.llm
	local NAME, LABEL, ENVS = "opencode-go", "opencode Go", { "OPENCODE_API_KEY" }

	local session_header = cfg.session_header
	local custom_tools = cfg.custom_tools == true

	local function routes()
		local out = {}
		for name, rc in pairs(type(cfg.routes) == "table" and cfg.routes or {}) do
			if type(rc) == "table" then
				out[name] = rc
			end
		end
		return out
	end
	local function route_names()
		local names = {}
		for name in pairs(routes()) do
			names[#names + 1] = name
		end
		table.sort(names)
		return table.concat(names, ", ")
	end

	local function api_key()
		return llm:api_key(NAME, ENVS)
	end

	-- The Responses route accepts the provider-level base URL override.
	local function responses_base(rc)
		return llm:base_url(NAME, cfg.base_url .. (rc.path or "/openai/v1"))
	end

	--- the address of `route` without the wire's own path
	local function route_base(route, rc)
		if route == "responses" then
			return responses_base(rc)
		end
		return cfg.base_url .. (rc.path or "")
	end

	--- the address of `route`: its base and the wire's path (`describe`); nil while the wire
	--- isn't registered
	local function route_url(route, rc, describe)
		local d = describe or llm:describe_wire(rc.wire)
		return d and (route_base(route, rc) .. (d.path or ""))
	end

	--- the endpoint of `route` for the request (the wire's `endpoint` table): the same fields
	--- on every route; the gateway takes the key as Bearer unless the route says otherwise
	local function endpoint(route, rc, req, key)
		local e = {
			provider = NAME,
			label = LABEL,
			route = route,
			url = route_url(route, rc, req.routes and req.routes.describe),
			key = key,
			auth = (req.routes and req.routes.auth) or rc.auth or "bearer",
			transport = "auto",
			headers = {},
			session_header = session_header,
			drop_fields = rc.drop_fields or {},
			custom_tools = custom_tools,
			route_cache_retention = rc.cache_retention,
			route_supports_long_cache_retention = rc.supports_long_cache_retention,
			route_supports_explicit_prompt_cache_mode = rc.supports_explicit_prompt_cache_mode,
			plugin_cache_retention = cfg.cache_retention,
			plugin_supports_long_cache_retention = cfg.supports_long_cache_retention,
		}
		if session_header and req.session_id then
			e.headers[session_header] = req.session_id
		end
		local m = req.model
		if m.replay_unsigned_reasoning ~= nil then
			e.replay_unsigned_reasoning = m.replay_unsigned_reasoning
		else
			e.replay_unsigned_reasoning = rc.replay_unsigned_reasoning == true
		end
		return e
	end

	--- why `model` can't be sent on its route, or nil: the route isn't configured, or its wire's
	--- plugin is disabled / failed
	local function unavailable_for(model)
		local route, src, _, route_error = llm:route_of(model)
		local rc = route and routes()[route]
		if not rc then
			return ("provider %s has no route %s (chosen by %s; configured: %s)"):format(
				NAME,
				tostring(route),
				tostring(src),
				route_names()
			)
		end
		-- registered? (the description, not the stream function: a status check needs no code)
		if llm:describe_wire(rc.wire) ~= nil then
			return route_error
		end
		-- the plugin declaring the wire (`wires = { … }` in its definition), even while it is off
		local plugin = llm:wire_owner(rc.wire)
		local why
		local st = plugin and mare.plugins and mare.plugins.status(plugin)
		if st and st.disabled then
			why = ("plugin %s is disabled"):format(plugin)
		elseif st and st.failure then
			why = ("plugin %s failed: %s"):format(plugin, tostring(st.failure.summary))
		end
		return ("provider %s route %s unavailable (%s)"):format(
			NAME,
			route,
			why or ("no plugin provides the %s wire"):format(tostring(rc.wire))
		)
	end

	local function stream(req, sink)
		local key = api_key()
		if not key then
			error({
				message = "opencode Go is not configured: run /login opencode-go or set OPENCODE_API_KEY",
				retryable = false,
				kind = "auth",
			})
		end
		local resolved = req.routes
		local route, rc, wire
		if resolved and resolved.config and resolved.stream then
			route, rc, wire = resolved.gateway, resolved.config, resolved.stream
		else
			local why = unavailable_for(req.model)
			if why then
				error({ message = why, retryable = false, kind = "invalid" })
			end
			route = llm:route_of(req.model)
			rc = routes()[route]
			wire = llm:wire(rc.wire)
		end
		local msg = wire(endpoint(route, rc, req, key), req, sink)
		msg.route = route
		return msg
	end

	-- the Anthropic route's effort mapping for the gateway's models: adaptive thinking with
	-- output_config.effort for thinking-capable models through this gateway;
	-- off disables thinking. The match outranks provider-anthropic's
	-- route-wide rules inside the provider layer.
	llm:define_params({
		match = NAME .. "/*",
		route = "anthropic",
		define = {
			effort = {
				options = { { value = "off", wire = { anthropic = { ["thinking.type"] = "disabled" } } } },
				wire = { anthropic = { ["thinking.type"] = "adaptive", ["output_config.effort"] = "$value" } },
			},
		},
	})

	local configured = {}
	for name, rc in pairs(routes()) do
		configured[name] = rc
	end

	llm:register_provider({
		provider = NAME,
		label = LABEL,
		models_dev = "opencode-go",
		login = {
			methods = { "api_key" },
			run = function(lio)
				llm:login_api_key(NAME, "opencode Go API key", lio)
			end,
		},
		catalog_target = function()
			return {
				url = mare.http.build_url(responses_base(routes().responses or {}) .. "/models"),
			}
		end,
		catalog = function(for_fetch)
			return {
				label = ("%s GET /models"):format(NAME),
				url = responses_base(routes().responses or {}) .. "/models",
				auth = "bearer",
				key = for_fetch and api_key() or nil,
				decode = function(raw)
					local items =
						require("mare_lib").decode_models_envelope(raw, "data", "id", ("%s GET /models"):format(NAME))
					local out = {}
					for _, m in ipairs(items) do
						out[#out + 1] = { id = m.id, name = type(m.name) == "string" and m.name or nil }
					end
					return out
				end,
			}
		end,
		login_label = "opencode Go subscription (API key)",
		default_model = "glm-5.3-flash",
		default_context = 1000000,
		default_max_output = 32000,
		-- shared by both routes: Responses call ids and Anthropic's ^[a-zA-Z0-9_-]+$ (the
		-- Anthropic wire cleans other characters) are at most 64 characters
		tool_id_policy = { max = 64 },
		default_reasoning = true,
		is_configured = function()
			return llm:has_api_key(NAME, ENVS)
		end,
		auth_mode = function()
			return llm:has_api_key(NAME, ENVS) and "api_key" or nil
		end,
		-- per-model routes (llm/routes)
		route = cfg.default_route or "responses",
		routes = configured,
		model_routes = cfg.model_routes,
		models_dev_routes = cfg.models_dev_routes == true,
		model_defaults = require("mare_lib").APPLY_PATCH_MODEL_DEFAULTS,
		unavailable_for = unavailable_for,
		stream = stream,
		errors = {
			codes = {
				goUsageLimitError = "quota",
				freeusagelimiterror = "quota",
			},
			map = function(raw)
				local msg = tostring(raw.message or "")
				if msg:find("MissingSessionID", 1, true) then
					return {
						kind = "invalid",
						retryable = false,
						message = msg
							.. (" — the gateway wants a %s header on every request (session_header)"):format(
								tostring(session_header)
							),
					}
				end
			end,
		},
		-- Offline fallback seeds keep only fields absent from the bundled models.dev snapshot
		-- (params, effort lists, effort_map); names, limits, modalities, reasoning flags and
		-- pricing are hydrated from preset/models_dev.json.gz (MIT). No `route`: built-in seeds
		-- stay on the default (responses) route unless routed via `model_routes` or `mare.model`.
		models = {
			{
				model_id = "glm-5.3-flash",
				-- the model layer's default effort: medium, chosen by the
				-- glm A/B — high wins on quality, but with the default prompt it multiplies
				-- thinking and cost (~4-5x, one 19-minute runaway); `/effort high` when wanted
				params = { effort = "medium" },
				-- opencode Go's Responses route does not require a placeholder reasoning item on
				-- cross-route history (unlike Chat Completions interleaved reasoning_content).
				needs_reasoning_placeholder = false,
			},
			{
				model_id = "glm-5.3",
				efforts = { "off", "low", "high", "max" },
			},
			{
				model_id = "deepseek-v4.1-flash",
				-- DeepSeek V4's levels are low|high|max (off → "none"); max is sent as max, never
				-- collapsed to high, and xhigh maps to max as DeepSeek's own API reads it.
				efforts = { "off", "low", "high", "max" },
				effort_map = { xhigh = "max" },
			},
			{
				model_id = "deepseek-v4-pro",
				efforts = { "off", "high", "max" },
				effort_map = { xhigh = "max" },
			},
			{
				model_id = "gpt-6-luna",
				efforts = { "off", "low", "medium", "high", "xhigh", "max" },
			},
			{
				model_id = "claude-haiku-5-5",
				efforts = { "off", "low", "medium", "high", "xhigh", "max" },
			},
			{
				model_id = "muse-spark-1.3-contributor",
				efforts = { "off", "minimal", "low", "medium", "high", "xhigh" },
			},
			{
				model_id = "mimo-v2.6-flash",
			},
		},
	})

	-- `mare doctor` / `/doctor`: each route's address and wire (a route whose wire isn't
	-- registered shows its base address).
	ctx:on("kernel/doctor", function(checks)
		local parts = {}
		for name, rc in pairs(routes()) do
			local url = route_url(name, rc)
			parts[#parts + 1] = ("%s → %s (%s%s)"):format(
				name,
				url or route_base(name, rc),
				tostring(rc.wire),
				url and "" or ", unavailable"
			)
		end
		table.sort(parts)
		checks[#checks + 1] = {
			name = "opencode Go routes",
			ok = true,
			detail = table.concat(parts, "; "),
		}
	end)
end
