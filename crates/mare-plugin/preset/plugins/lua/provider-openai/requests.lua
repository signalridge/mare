-- provider-openai.requests: endpoints, streaming and /responses/compact requests, the openai and
-- `wire_api = "responses"` provider registrations and the "openai-responses" wire (M.stream);
-- part of the `provider-openai` plugin (plugins/provider-openai.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg, M = D.ctx, D.cfg, D.M
	local lib = require("mare_lib")
	-- tools is optional: followed, never read undeclared
	local follow = lib.followers(ctx, { "tools" })
	local llm, oauth, api_key, has_key, fresh_oauth =
		D.auth.llm, D.auth.oauth, D.auth.api_key, D.auth.has_key, D.auth.fresh_oauth
	local has_oauth = D.auth.has_oauth
	local login, revoke = D.login.spec, D.login.revoke

	-- the Responses route's parameter mappings: the effort
	-- level as `reasoning.effort`, off as "none"; verbosity as
	-- `text.verbosity`, for models whose metadata has one
	llm:define_params({
		route = "responses",
		define = {
			effort = {
				options = { { value = "off", wire = { responses = { ["reasoning.effort"] = "none" } } } },
				wire = { responses = { ["reasoning.effort"] = "$value" } },
			},
			verbosity = { wire = { responses = { ["text.verbosity"] = "$value" } } },
		},
	})
	-- `verbosity = "…"` in this plugin's config overrides every model's default (user layer)
	if cfg.verbosity ~= nil then
		llm:define_params({
			route = "responses",
			where = "plugins.provider-openai.verbosity",
			define = { verbosity = { default = cfg.verbosity } },
		}, "user")
	end

	local ensure_codex_models = lib.lazy(function()
		return require("provider-openai.codex_models")
	end)

	local function decode_openai_models(name)
		return function(raw)
			local _, lossless_data =
				require("mare_lib").decode_models_envelope(raw, "data", "id", ("%s GET /models"):format(name))
			local out = {}
			for _, m in ipairs(lossless_data) do
				out[#out + 1] = { id = m.id, name = type(m.name) == "string" and m.name or nil }
			end
			return out
		end
	end

	local function openai_catalog_descriptor(name, base_fn, key_fn, include_snapshot)
		return function(for_fetch)
			local base = base_fn()
			local key = for_fetch and key_fn() or nil
			return {
				label = ("%s GET /models"):format(name),
				url = base .. "/models",
				auth = "bearer",
				key = key,
				decode = decode_openai_models(name),
				snapshot = include_snapshot and function()
					return {
						auth = (key and key ~= "") and ("api_key:" .. mare.crypto.sha256(key)) or nil,
						target = {
							mode = "api_key",
							url = mare.http.build_url(base .. "/models"),
						},
					}
				end or nil,
			}
		end
	end

	local function codex_request_snapshot(access_token, account_id)
		local codex_models = ensure_codex_models()
		local base, berr = codex_models.valid_base(cfg.codex_base_url)
		if not base then
			return nil, berr
		end
		local headers = {
			["chatgpt-account-id"] = account_id or "",
			["openai-beta"] = "responses=experimental",
			["originator"] = cfg.originator,
			["accept"] = "application/json",
		}
		local auto_auth = (type(access_token) == "string" and access_token ~= "") and ("Bearer " .. access_token) or nil
		if auto_auth then
			headers["authorization"] = auto_auth
		end
		for k, v in pairs(cfg.headers or {}) do
			if v ~= nil and string.lower(tostring(k)) == "authorization" then
				for existing in pairs(headers) do
					if string.lower(tostring(existing)) == "authorization" then
						headers[existing] = nil
					end
				end
			end
			headers[k] = v
		end
		local url = mare.http.build_url(base .. "/codex/models", { client_version = codex_models.client_version() })
		local wire = { provider = "openai", url = url, headers = headers }
		wire = ctx.bus:waterfall("provider/request", wire) or wire
		if auto_auth and type(wire) == "table" and type(wire.headers) == "table" then
			for k, v in pairs(wire.headers) do
				if k ~= "authorization" and v ~= nil and string.lower(tostring(k)) == "authorization" then
					if wire.headers["authorization"] == auto_auth then
						wire.headers["authorization"] = nil
					end
					break
				end
			end
		end
		return wire, nil, url, auto_auth
	end

	local function codex_auth_scope(o)
		if type(o) ~= "table" then
			return "chatgpt:" .. mare.crypto.sha256("")
		end
		local key
		if o.account_id or o.email then
			-- Hash both workspace account_id and email so users sharing one workspace stay isolated
			-- across token refreshes, while token rotation for the same user stays a cache hit.
			key = mare.json.encode_strict({ tostring(o.account_id or ""), tostring(o.email or "") })
		else
			key = tostring(o.refresh or o.access or "")
		end
		return "chatgpt:" .. mare.crypto.sha256(key)
	end

	local function codex_target_from_wire(wire, auto_auth)
		if not wire then
			return { mode = "chatgpt", url = cfg.codex_base_url }
		end
		local header_digests = {}
		for k, v in pairs(wire.headers or {}) do
			local lk = string.lower(tostring(k))
			if v ~= nil and (lk ~= "authorization" or tostring(v) ~= auto_auth) then
				header_digests[lk] = mare.crypto.sha256(tostring(v))
			end
		end
		return {
			mode = "chatgpt",
			url_digest = wire.url and mare.crypto.sha256(tostring(wire.url)) or nil,
			headers = next(header_digests) and header_digests or nil,
			proxy = wire.proxy,
		}
	end

	--- Declarative GET {codex_base_url}/codex/models?client_version=... descriptor for ChatGPT OAuth.
	local function codex_catalog_descriptor(for_fetch)
		local codex_models = ensure_codex_models()
		local pc = llm:provider_config("openai") or {}
		local base, berr = codex_models.valid_base(cfg.codex_base_url)
		if not base then
			if for_fetch then
				error(berr, 0)
			end
			return { label = "openai GET /codex/models", url = cfg.codex_base_url or "" }
		end
		local a
		if for_fetch then
			local ok_auth, res_a = pcall(fresh_oauth, false, nil, {
				proxy = pc.proxy,
				connect_timeout_ms = pc.connect_timeout_ms,
			})
			if not ok_auth then
				error(type(res_a) == "table" and res_a.message or tostring(res_a), 0)
			end
			a = res_a
			if not a or type(a.access) ~= "string" or a.access == "" then
				return nil
			end
		else
			a = oauth()
		end
		local headers = {
			["chatgpt-account-id"] = (a and a.account_id) or "",
			["openai-beta"] = "responses=experimental",
			["originator"] = cfg.originator,
		}
		for k, v in pairs(cfg.headers or {}) do
			headers[k] = v
		end
		local url = mare.http.build_url(base .. "/codex/models", { client_version = codex_models.client_version() })
		return {
			label = "openai GET /codex/models",
			url = url,
			headers = headers,
			auth = "bearer",
			key = for_fetch and a.access or nil,
			check_origin = true,
			validate_url = codex_models.valid_base,
			decode = function(raw)
				local lossless = mare.json.decode_lossless(raw)
				if
					type(lossless) ~= "table"
					or codex_models.is_array(lossless)
					or codex_models.has_error_field(lossless)
					or not codex_models.validate_models_payload(lossless.models)
				then
					error("openai GET /codex/models: invalid response", 0)
				end
				local j = mare.json.decode(raw)
				if type(j) ~= "table" or type(j.models) ~= "table" then
					error("openai GET /codex/models: invalid response", 0)
				end
				local out, seen = {}, {}
				for _, m in ipairs(j.models) do
					local entry = codex_models.entry(m)
					if entry and not seen[entry.id] then
						seen[entry.id] = true
						out[#out + 1] = entry
					end
				end
				return out
			end,
			snapshot = function(wire, auto_auth)
				return {
					auth = codex_auth_scope(a),
					target = codex_target_from_wire(wire, auto_auth),
				}
			end,
		}
	end

	local function use_oauth()
		local cred = llm:active_credential("openai", { "OPENAI_API_KEY" })
		return cred ~= nil and cred.kind == "oauth"
	end

	local function merge_cfg_headers(base)
		local h = {}
		for k, v in pairs(base or {}) do
			h[k] = v
		end
		for k, v in pairs(cfg.headers or {}) do
			h[k] = v
		end
		return h
	end

	local function normalize_hosted_tools(src, default_supported)
		local ht = type(src.hosted_tools) == "table" and src.hosted_tools or nil
		local ws = ht and ht.web_search or nil
		local supported, mode, config
		if type(ws) == "table" then
			supported, mode, config = ws.supported, ws.mode, ws.config
		elseif type(ws) == "string" then
			mode = ws
		elseif type(ws) == "boolean" then
			supported = ws
		end
		if src.supports_web_search ~= nil and supported == nil then
			supported = src.supports_web_search == true
		end
		if mode == nil then
			mode = src.web_search
		end
		if config == nil then
			config = src.web_search_config
		end
		if supported == nil then
			supported = default_supported == true or (mode ~= nil and mode ~= "disabled")
		end
		return {
			web_search = {
				supported = supported == true,
				mode = mode,
				config = config,
			},
		}
	end

	local OPENAI_HOSTED_TOOLS = normalize_hosted_tools(cfg, true)

	local function openai_endpoint(_req)
		if use_oauth() then
			local o = oauth()
			if o then
				local a = fresh_oauth()
				local cred_parts = {}
				if type(a) == "table" then
					for _, k in ipairs({ "id", "account_id", "email" }) do
						if a[k] ~= nil and tostring(a[k]) ~= "" then
							cred_parts[#cred_parts + 1] = tostring(a[k])
						end
					end
				end
				local cred_id = #cred_parts > 0 and table.concat(cred_parts, ":") or nil
				return {
					provider = "openai",
					label = "OpenAI",
					url = cfg.codex_base_url .. "/codex/responses",
					auth = "bearer",
					key = a.access,
					credential_id = cred_id and ("oauth:" .. cred_id) or nil,
					headers = merge_cfg_headers({
						["chatgpt-account-id"] = a.account_id or "",
						["openai-beta"] = "responses=experimental",
						["originator"] = cfg.originator,
						["accept"] = "text/event-stream",
					}),
					codex = true,
					codex_headers = true,
					official_openai = true,
					websocket = true,
					transport = cfg.transport,
					idle_timeout_ms = cfg.stream_idle_timeout_ms,
					hosted_tools = OPENAI_HOSTED_TOOLS,
					on_unauthorized = function(ev)
						-- on 401 re-read credentials from disk (another process may have rotated the refresh
						-- token); if unchanged, force a refresh; then let send.lua re-encode and retry once
						local used = ev.wire and ev.wire.headers and ev.wire.headers["authorization"]
						local disk = oauth()
						if disk and ("Bearer " .. disk.access) == used then
							local okr, rerr = pcall(fresh_oauth, true)
							if not okr then
								mare.log("warn", "openai: token refresh after a 401 failed: " .. tostring(rerr))
							end
						end
						return true
					end,
				}
			end
		end
		local key = api_key()
		if not key then
			error({
				message = "OpenAI is not configured: run /login openai or set OPENAI_API_KEY",
				retryable = false,
			})
		end
		return {
			provider = "openai",
			label = "OpenAI",
			url = llm:base_url("openai", cfg.base_url or "https://api.openai.com/v1") .. "/responses",
			auth = "bearer",
			key = key,
			headers = merge_cfg_headers({ ["accept"] = "text/event-stream" }),
			codex = false,
			codex_headers = true,
			official_openai = true,
			websocket = true,
			transport = cfg.transport,
			idle_timeout_ms = cfg.stream_idle_timeout_ms,
			hosted_tools = OPENAI_HOSTED_TOOLS,
		}
	end

	-- Keep endpoint credentials away from parameter mapping functions. Custom endpoint headers
	-- are retained, and the existing project mapping guard still protects credential names.
	local function take_auth(headers)
		local auth = {}
		for k, v in pairs(headers) do
			local name = type(k) == "string" and string.lower(k)
			if name == "authorization" or name == "x-api-key" or name == "x-goog-api-key" then
				auth[k], headers[k] = v, nil
			end
		end
		return auth
	end

	local function put_auth(headers, auth)
		for k, v in pairs(auth) do
			for existing in pairs(headers) do
				if type(existing) == "string" and string.lower(existing) == string.lower(k) then
					headers[existing] = nil
				end
			end
			headers[k] = v
		end
	end

	-- x-codex-turn-state token keyed by the active main turn (Codex core/src/client.rs:388).
	-- Auxiliary requests (side, title, branch_summary, etc.) neither send nor capture it, and
	-- late responses from an earlier turn cannot populate a newer turn's slot.
	local turn_gen = 0
	local active_turn_key = "turn:0"
	local turn_states = {}
	ctx:on("turn/started", function(ev)
		turn_gen = turn_gen + 1
		active_turn_key = (type(ev) == "table" and type(ev.turn_id) == "string" and ev.turn_id ~= "") and ev.turn_id
			or ("turn:" .. tostring(turn_gen))
		turn_states = {}
	end)
	ctx:on("turn/completed", function(ev)
		local tid = type(ev) == "table" and ev.turn_id or nil
		if tid == nil or tid == active_turn_key then
			turn_gen = turn_gen + 1
			active_turn_key = nil
			turn_states = {}
		end
	end)
	ctx:on("session/changed", function()
		turn_gen = turn_gen + 1
		active_turn_key = nil
		turn_states = {}
	end)
	local function turn_key_for(req)
		if type(req) ~= "table" or req.purpose ~= "agent" then
			return nil
		end
		if type(req.turn_id) == "string" and req.turn_id ~= "" then
			return req.turn_id
		end
		return active_turn_key
	end

	-- the provider this request goes to: OpenAI (API key or ChatGPT subscription) by default,
	-- or another Responses endpoint (such as opencode Go via M.stream below)
	local OPENAI = {
		name = "openai",
		label = "OpenAI",
		codex_headers = true,
		official_openai = true,
		websocket = true,
		plugin_cfg = cfg,
		hosted_tools = OPENAI_HOSTED_TOOLS,
	}
	local function session_value(req)
		return req.session_id
	end

	local function is_openai_host(url)
		local host = type(url) == "string" and url:match("^https?://([^/:]+)") or nil
		if type(host) ~= "string" then
			return false
		end
		host = host:lower()
		return host == "api.openai.com" or host == "chatgpt.com"
	end

	local function is_explicit_cache_model(model)
		local id = type(model) == "table" and type(model.id) == "string" and model.id:lower() or ""
		if id:match("^gpt%-5%.[6-9]") or id:match("^gpt%-6") or id:match("^gpt%-7") then
			return true
		end
		return type(model) == "table" and type(model.cost) == "table" and (model.cost.cache_write or 0) > 0
	end

	local function is_in_memory_cache_model(model)
		local id = type(model) == "table" and type(model.id) == "string" and model.id:lower() or ""
		-- GPT-5.5 only supports 24h retention (not in_memory); legacy GPT-5.1/5.2/5.4/4.1/4o/o3/o4 support in_memory.
		if id:match("^gpt%-5%.5") then
			return false
		end
		return id:match("^gpt%-5%.[0-4]") ~= nil or id:match("^gpt%-4") ~= nil or id:match("^o[34]") ~= nil
	end

	local function apply_prompt_cache(body, prov, req, model, codex, url, is_compact)
		local lib = require("mare_lib")
		local prov_name = prov.provider or prov.name
		local prov_cfg = prov.provider_config or (prov_name and (mare.config.providers or {})[prov_name] or nil)
		local route_cfg = prov.route_cfg
			or (
				(
						prov.route_cache_retention ~= nil
						or prov.route_supports_long_cache_retention ~= nil
						or prov.route_supports_explicit_prompt_cache_mode ~= nil
					)
					and {
						cache_retention = prov.route_cache_retention,
						supports_long_cache_retention = prov.route_supports_long_cache_retention,
						supports_explicit_prompt_cache_mode = prov.route_supports_explicit_prompt_cache_mode,
					}
				or nil
			)
		local plugin_cfg = prov.plugin_cfg
			or (prov_name == "openai" and (prov.wire_config or cfg))
			or (
				(prov.plugin_cache_retention ~= nil or prov.plugin_supports_long_cache_retention ~= nil)
					and {
						cache_retention = prov.plugin_cache_retention,
						supports_long_cache_retention = prov.plugin_supports_long_cache_retention,
					}
				or nil
			)
		local retention, read_only, supports_long, source =
			lib.resolve_cache_retention(req, model, prov_cfg, plugin_cfg, route_cfg, prov.global_config)
		local is_openai_endpoint = (prov.official_openai == true) or is_openai_host(url)
		local explicit_mode =
			lib.resolve_cache_cap("supports_explicit_prompt_cache_mode", nil, model, prov_cfg, route_cfg, plugin_cfg)
		if explicit_mode == nil and is_openai_endpoint then
			explicit_mode = is_explicit_cache_model(model)
		end
		local supports_in_memory =
			lib.resolve_cache_cap("supports_in_memory_cache_retention", nil, model, prov_cfg, route_cfg, plugin_cfg)
		if supports_in_memory == nil and is_openai_endpoint and not explicit_mode then
			supports_in_memory = is_in_memory_cache_model(model)
		end

		if explicit_mode and not codex then
			local effective_long = supports_long and not is_compact
			if retention == "none" then
				body.prompt_cache_options = { mode = "explicit" }
			elseif retention == "long" and not read_only and effective_long then
				if req.session_id then
					body.prompt_cache_key = req.session_id
				end
				-- 12h for long vs 30m default.
				body.prompt_cache_options = { ttl = "12h" }
			else
				if req.session_id then
					body.prompt_cache_key = req.session_id
				end
				if retention == "long" and not read_only and not effective_long and source and source ~= "default" then
					local reason = is_compact
							and "/responses/compact only supports 30m prompt cache TTL; using short default"
						or "does not support long prompt cache retention; using short default"
					lib.warn_cache_limitation(
						ctx,
						("provider-openai: %s/%s %s"):format(
							tostring(prov_name or "openai"),
							tostring(model.id or "?"),
							reason
						)
					)
				end
			end
			return
		end
		if retention == "none" then
			if req.cache ~= false then
				lib.warn_cache_limitation(
					ctx,
					("provider-openai: %s/%s does not support disabling server-side automatic prompt caching; omitting prompt_cache_key only"):format(
						tostring(prov_name or "openai"),
						tostring(model.id or "?")
					)
				)
			end
			return
		end
		if req.session_id then
			body.prompt_cache_key = req.session_id
		end
		if codex or read_only then
			return
		end
		if retention == "long" then
			if supports_long then
				body.prompt_cache_retention = "24h"
			elseif source and source ~= "default" then
				lib.warn_cache_limitation(
					ctx,
					("provider-openai: %s/%s does not support long prompt cache retention"):format(
						tostring(prov_name or "openai"),
						tostring(model.id or "?")
					)
				)
			end
		elseif retention == "short" then
			if supports_in_memory and supports_long then
				body.prompt_cache_retention = "in_memory"
			elseif source and source ~= "default" and is_openai_endpoint then
				lib.warn_cache_limitation(
					ctx,
					("provider-openai: %s/%s does not support in_memory prompt cache retention; omitting prompt_cache_retention"):format(
						tostring(prov_name or "openai"),
						tostring(model.id or "?")
					)
				)
			end
		end
	end

	-- Session headers: session-id / thread-id, the request id, the sticky-routing token of this
	-- turn and the sub-agent kind.
	local function encode(req, ep)
		local prov_name = ep.provider or ep.name or "openai"
		local url = ep.url
		local codex = ep.codex == true
		local headers = { ["accept"] = "text/event-stream" }
		local model = req.model
		local schema_sanitize = require("mare_lib.schema_sanitize")
		local oai_dialect = schema_sanitize.detect_dialect(ep.dialect, ep.base_url, model.id)
		local placeholder_reasoning = lib.needs_reasoning_placeholder(
			model,
			ep,
			oai_dialect.deepseek == true
				and not ep.official_openai
				and not oai_dialect.grok
				and ep.dialect ~= "openai"
				and ep.dialect ~= "azure",
			llm:model_sources(model.ref)
		)
		local function lookup_tool(name)
			return follow.tools and follow.tools:get(name)
		end
		local reg_prov = prov_name and llm:provider(prov_name) or nil
		local ht = ep.hosted_tools or (reg_prov and reg_prov.hosted_tools) or nil
		local ws_decl = type(ht) == "table" and ht.web_search or nil
		local ws_supported, ws_decl_mode, ws_decl_cfg = false, nil, nil
		if type(ws_decl) == "table" then
			ws_supported = ws_decl.supported == true
				or (ws_decl.supported == nil and ws_decl.mode ~= nil and ws_decl.mode ~= "disabled")
			ws_decl_mode, ws_decl_cfg = ws_decl.mode, ws_decl.config
		elseif type(ws_decl) == "string" then
			ws_supported = ws_decl ~= "disabled"
			ws_decl_mode = ws_decl
		elseif ws_decl == true then
			ws_supported = true
		end
		local function web_search_allowed()
			if req.tool_choice == "none" or req.purpose == "side" then
				return false
			end
			if mare.subagent and mare.subagent.tool_allowed and not mare.subagent.tool_allowed("web_search") then
				return false
			end
			local only = mare.config and mare.config.tools_enabled
			if type(only) == "table" and not table.find(only, "web_search") then
				return false
			end
			if follow.tools and follow.tools.is_disabled and follow.tools:is_disabled("web_search") then
				return false
			end
			return true
		end
		local ws_allowed = web_search_allowed()
		local tools, explicit_ws, wire_tools_by_name, client_tool_search = M.convert_tools(req.tools, lookup_tool, {
			custom = ep.custom_tools ~= false,
			dialect = oai_dialect,
			defer_loading = model.tool_update == "addition_only",
			allow_hosted = req.tool_choice ~= "none",
			web_search_supported = ws_supported and ws_allowed,
			web_search_default_mode = ws_decl_mode,
			web_search_default_config = ws_decl_cfg,
		})
		if ws_allowed and not explicit_ws and ws_supported and ws_decl_mode ~= nil and ws_decl_mode ~= "disabled" then
			local ws_tool = M.hosted_web_search_tool(ws_decl_mode, ws_decl_cfg)
			if ws_tool then
				tools[#tools + 1] = ws_tool
			end
		end
		local healing_pattern = oai_dialect.deepseek == true and "dsml" or nil
		local body = {
			model = model.id,
			instructions = req.system ~= "" and req.system or "You are a helpful assistant.",
			input = M.convert_messages(req.messages, model.id, prov_name, {
				has_tools = type(req.tools) == "table" and #req.tools > 0,
				placeholder_reasoning = placeholder_reasoning,
				lookup = lookup_tool,
				custom = ep.custom_tools ~= false,
				dialect = oai_dialect,
				wire_tools_by_name = wire_tools_by_name,
				client_tool_search = client_tool_search,
				markup_healing = healing_pattern,
			}),
			stream = true,
			store = false,
			tool_choice = req.tool_choice == "none" and "none" or "auto",
			parallel_tool_calls = true,
			include = { "reasoning.encrypted_content" },
		}
		apply_prompt_cache(body, ep, req, model, codex, url)
		local tkey, tgen = turn_key_for(req), turn_gen
		if ep.codex_headers then
			if req.session_id then
				headers["session-id"] = req.session_id
				headers["thread-id"] = req.session_id
				headers["x-client-request-id"] = req.session_id
			end
			if tkey and turn_states[tkey] then
				headers["x-codex-turn-state"] = turn_states[tkey]
			end
			local subagent = req.subagent or (mare.mode == "child" and "collab_spawn" or nil)
			if subagent then
				headers["x-openai-subagent"] = subagent
			end
		end
		-- endpoints that want the session id in their own header
		local sh = ep.session_header or (ep.official_openai and cfg.session_header)
		if sh and session_value(req) then
			headers[sh] = session_value(req)
		end
		if #tools > 0 then
			body.tools = tools
		end
		local req_native = type(req.native) == "table"
				and type(req.native["openai-responses"]) == "table"
				and req.native["openai-responses"]
			or nil
		if req_native then
			if type(req_native.service_tier) == "string" and req_native.service_tier ~= "" then
				body.service_tier = req_native.service_tier
			end
			if type(req_native.include) == "table" then
				local seen = {}
				for _, inc in ipairs(body.include) do
					seen[inc] = true
				end
				for _, inc in ipairs(req_native.include) do
					if type(inc) == "string" and inc ~= "" and not seen[inc] then
						seen[inc] = true
						body.include[#body.include + 1] = inc
					end
				end
			end
		end
		-- Seed the encoded cap before merging parameter mappings, so `at_least` compares with
		-- an explicit cap as well as the calculated one. ChatGPT login sends no cap.
		if not codex and (not req.max_tokens or req.params_overlay) then
			body.max_output_tokens = req.max_tokens or M.output_cap(model, body.instructions, body.input, body.tools)
		end
		if req.output_schema ~= nil then
			local out_dialect = schema_sanitize.detect_dialect(
				ep.dialect,
				ep.base_url,
				model.id,
				{ output_schema = true, strict = true }
			)
			body.text = body.text or {}
			body.text.format = {
				type = "json_schema",
				name = "mare_output_schema",
				strict = true,
				schema = schema_sanitize.sanitize(req.output_schema, out_dialect),
			}
		end
		local can_cap_reply = not codex
		for _, f in ipairs(ep.drop_fields or {}) do
			if f == "max_output_tokens" then
				can_cap_reply = false
			end
		end
		return {
			method = "POST",
			url = url,
			headers = headers,
			body = body,
			model = model.id,
		}, {
			codex = codex,
			no_reply_left = not can_cap_reply,
			markup_healing = healing_pattern,
			turn_key = tkey,
			turn_gen = tgen,
		}
	end

	local function finalize(req, ep, wire, st)
		local body, model = wire.body, req.model
		local codex = type(st) == "table" and st.codex == true
		local summary_cfg = (ep.wire_config and ep.wire_config.reasoning_summary) or cfg.reasoning_summary or "auto"
		if body.reasoning and model.reasoning_summary then
			body.reasoning.summary = summary_cfg
		end
		-- A prepared parameter mapping is already the request's effective output cap.
		local mapped_cap = req.params_overlay and req.params_overlay.body.max_output_tokens
		if not codex and req.max_tokens and mapped_cap == nil then
			body.max_output_tokens = req.max_tokens
		end
		for _, f in ipairs(ep.drop_fields or {}) do
			body[f] = nil
		end
	end

	local function header_ci(hdrs, target)
		if type(hdrs) ~= "table" then
			return nil
		end
		local direct = hdrs[target]
		if type(direct) == "string" and direct ~= "" then
			return direct
		end
		for k, v in pairs(hdrs) do
			if type(k) == "string" and k:lower() == target and type(v) == "string" and v ~= "" then
				return v
			end
		end
		return nil
	end

	local function capture_turn_state(hdrs, tkey, tgen)
		if tkey == nil or tgen ~= turn_gen then
			return
		end
		local existing = turn_states[tkey]
		if existing ~= nil and existing ~= "" then
			return
		end
		local ts = header_ci(hdrs, "x-codex-turn-state")
		if ts then
			turn_states[tkey] = ts
		end
	end

	local function decoder(sink, context)
		local ep = context.endpoint or {}
		local resp = context.response or {}
		local st = type(context.state) == "table" and context.state or {}
		local prov_name = ep.provider or ep.name or "openai"
		local function capture_for_req(hdrs)
			capture_turn_state(hdrs, st.turn_key, st.turn_gen)
		end
		-- sticky routing (Codex x-codex-turn-state): replayed on every request of this turn
		capture_for_req(resp.headers)
		local limits = M.parse_rate_limits(resp.headers or {})
		if limits then
			limits.received_at_ms = require("mare_lib").to_epoch_ms(mare.task.now())
			ctx.bus:emit("llm/rate-limits", { provider = prov_name, limits = limits })
		end
		local P = M.new_parser(sink, function(raw)
			return { source = "stream", payload = raw }
		end, {
			on_metadata = capture_for_req,
			markup_healing = st.markup_healing,
		})
		return {
			event = P.feed,
			finish = function()
				local msg = P.finish()
				msg.subscription = st.codex or nil -- ChatGPT plan: no per-token cost
				return msg
			end,
		}
	end

	local function responses_provider_factory(name, entry)
		local envs = (type(entry.api_key_env) == "string" and entry.api_key_env ~= "") and { entry.api_key_env } or {}
		local cfg_has_key = type(((mare.config.providers or {})[name] or {}).api_key) ~= "nil"
		local raw_key = not cfg_has_key and type(entry.api_key) == "string" and entry.api_key ~= "" and entry.api_key
			or nil
		local hosted = normalize_hosted_tools(entry, false)
		local function api_key()
			return raw_key or llm:api_key(name, envs)
		end
		local function live_entry()
			local eff = llm:provider_config(name)
			if type(eff) == "table" and next(eff) ~= nil then
				return eff
			end
			return entry
		end
		local function live_base_url(eff)
			local raw = llm:base_url(name, (eff and eff.base_url) or entry.base_url)
			return tostring(raw or ""):gsub("/+$", "")
		end
		local function custom_endpoint(_req)
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
			local merged_headers = { ["accept"] = "text/event-stream" }
			for k, v in pairs(eff.headers or {}) do
				merged_headers[k] = v
			end
			local base = live_base_url(eff)
			return {
				provider = name,
				label = eff.label or name,
				dialect = eff.dialect,
				base_url = base,
				url = base .. "/responses",
				auth = "bearer",
				key = key,
				headers = merged_headers,
				custom_tools = eff.custom_tools == true,
				hosted_tools = hosted,
				drop_fields = eff.drop_fields,
				transport = eff.transport,
				endpoint_config = eff,
			}
		end
		return {
			route = "responses",
			wire = "openai-responses",
			endpoint = custom_endpoint,
			warm_url = function()
				return live_base_url(live_entry())
			end,
			stream = function(req, sink)
				return M.stream(custom_endpoint, req, sink)
			end,
			no_key = entry.no_key or nil,
			login = not entry.no_key and {
				methods = { "api_key" },
				run = function(lio)
					llm:login_api_key(name, nil, lio)
				end,
			} or nil,
			login_label = "API key",
			tool_id_policy = { max = 64 },
			default_reasoning = entry.reasoning ~= false,
			hosted_tools = hosted,
			model_defaults = require("mare_lib").APPLY_PATCH_MODEL_DEFAULTS,
			is_configured = function()
				return entry.no_key or raw_key ~= nil or llm:has_api_key(name, envs)
			end,
			catalog_target = entry.list_models ~= false and function()
				local eff = live_entry()
				return {
					url = mare.http.build_url(live_base_url(eff) .. "/models"),
					headers = eff.headers,
				}
			end or nil,
			catalog_scope = (entry.list_models ~= false and raw_key ~= nil) and function()
				return "api_key:" .. mare.crypto.sha256(raw_key)
			end or nil,
			catalog = entry.list_models ~= false and function(for_fetch)
				local eff = live_entry()
				local headers = {}
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
					decode = decode_openai_models(name),
				}
			end or nil,
		}
	end

	llm:register_wire({
		name = "openai-responses",
		config = cfg,
		route = "responses",
		path = "/responses",
		auth = { "bearer" },
		errors = M.ERRORS,
		estimate_hints = {
			image_types = { "input_image" },
			capped_fields = { encrypted_content = 4800 },
		},
		encode = encode,
		finalize = finalize,
		decoder = decoder,
		hosted_tools = { "web_search" },
		provider = responses_provider_factory,
		provider_fields = {
			custom_tools = "boolean",
			supports_long_cache_retention = "boolean",
			supports_explicit_prompt_cache_mode = "boolean",
			supports_in_memory_cache_retention = "boolean",
			needs_reasoning_placeholder = "boolean",
			supports_web_search = "boolean",
			web_search = "string",
			web_search_config = "table",
		},
		model_fields = {
			reasoning_summary = "boolean",
			verbosity = "string",
			needs_reasoning_placeholder = "boolean",
			supports_long_cache_retention = "boolean",
			supports_explicit_prompt_cache_mode = "boolean",
			supports_in_memory_cache_retention = "boolean",
		},
		enrich_model = function(m, entry, mark)
			if m.needs_reasoning_placeholder == nil and type(entry) == "table" then
				local il = entry.interleaved
				if il == "reasoning_content" or (type(il) == "table" and il.field == "reasoning_content") then
					m.needs_reasoning_placeholder = true
					mark("needs_reasoning_placeholder")
				end
			end
		end,
	})
	M.stream = llm:wire("openai-responses")

	local function stream(req, sink)
		return M.stream(openai_endpoint, req, sink)
	end

	--- /responses/compact: the server compacts
	--- the whole input into a replacement history. Returns {items, model} | nil, err.
	local function build_compact_wire(req)
		local ep = openai_endpoint(req)
		local url, codex = ep.url .. "/compact", ep.codex == true
		local headers = {}
		for k, v in pairs(ep.headers or {}) do
			headers[k] = v
		end
		headers["accept"] = "application/json"
		local auth = take_auth(headers)
		if not auth["authorization"] and ep.key then
			auth["authorization"] = "Bearer " .. ep.key
		end
		local model = req.model
		local instructions = req.system ~= "" and req.system or "You are a helpful assistant."
		local function lookup_tool(name)
			return follow.tools and follow.tools:get(name)
		end
		local schema_sanitize = require("mare_lib.schema_sanitize")
		local oai_dialect = schema_sanitize.detect_dialect(ep.dialect, url, model and model.id)
		local tools, _, wire_tools_by_name, client_tool_search = M.convert_tools(req.tools or {}, lookup_tool, {
			custom = ep.custom_tools ~= false,
			dialect = oai_dialect,
			defer_loading = model and model.tool_update == "addition_only",
		})
		local input = M.trim_outputs_to_window(
			M.convert_messages(req.messages, model.id, ep.provider or ep.name or "openai", {
				lookup = lookup_tool,
				custom = ep.custom_tools ~= false,
				dialect = oai_dialect,
				wire_tools_by_name = wire_tools_by_name,
				client_tool_search = client_tool_search,
			}),
			instructions,
			model
		)
		local body = {
			model = model.id,
			instructions = instructions,
			input = input,
			parallel_tool_calls = true,
		}
		if #tools > 0 then
			body.tools = tools
		end
		local guard_tools = require("mare_lib").guard_tool_declarations(body)
		llm:apply_params(req, { body = body, headers = headers }, { only = { effort = true } })
		local mapped_headers = headers
		headers = {}
		for name, value in next, mapped_headers do
			headers[name] = value
		end
		put_auth(headers, auth)
		apply_prompt_cache(body, OPENAI, req, model, codex, url, true)
		if req.session_id then
			headers["session-id"] = req.session_id
			headers["thread-id"] = req.session_id
			if cfg.session_header then
				headers[cfg.session_header] = req.session_id
			end
		end
		local tkey, tgen = active_turn_key, turn_gen
		if tkey and turn_states[tkey] then
			headers["x-codex-turn-state"] = turn_states[tkey]
		end
		for k, v in pairs(cfg.headers or {}) do
			headers[k] = v
		end
		local pc = llm:provider_config("openai") or {}
		local wire = {
			provider = "openai",
			url = url,
			headers = headers,
			body = body,
			model = model.id,
			compact = true,
			proxy = pc.proxy,
			connect_timeout_ms = pc.connect_timeout_ms,
		}
		wire = ctx.bus:waterfall("provider/request", wire) or wire
		guard_tools(wire)
		return ep, wire, model, codex, tkey, tgen
	end

	local function compact_http_kind(status)
		if status == 401 or status == 403 then
			return "auth"
		elseif status == 429 then
			return "rate_limit"
		elseif status == 413 then
			return "payload_too_large"
		elseif status and status >= 400 and status < 500 then
			return "invalid"
		end
		return "server"
	end

	local function compact(req)
		local pc = llm:provider_config("openai") or {}
		local ep, wire, model, codex, tkey, tgen = build_compact_wire(req)
		local ok, resp
		for attempt = 1, 2 do
			ok, resp = pcall(mare.http.request, {
				method = "POST",
				url = wire.url,
				headers = wire.headers,
				json = wire.body,
				cancel = req.cancel,
				fresh = req.fresh_connection,
				proxy = wire.proxy or ep.proxy or pc.proxy,
				connect_timeout_ms = wire.connect_timeout_ms or pc.connect_timeout_ms,
				timeout_ms = wire.timeout_ms or cfg.compact_timeout_ms,
				idle_timeout_ms = req.idle_timeout_ms or ep.idle_timeout_ms or cfg.stream_idle_timeout_ms,
				header_timeout_ms = req.header_timeout_ms,
				redirect = "same-origin",
			})
			if not ok then
				break
			end
			if resp.status == 401 and attempt == 1 and type(ep.on_unauthorized) == "function" then
				pcall(resp.close, resp)
				local ok_u, should_retry = pcall(ep.on_unauthorized, { wire = wire })
				if ok_u and should_retry then
					ep, wire, model, codex, tkey, tgen = build_compact_wire(req)
				else
					break
				end
			else
				break
			end
		end
		if not ok then
			if req.cancel and req.cancel:is_cancelled() then
				return nil, "cancelled", "cancelled"
			end
			local e = mare.text.redact(tostring(resp))
			-- a network policy refusal stops compaction; it isn't a reason to try another way
			local denied = e:find("not allowed", 1, true) or e:find("denied", 1, true)
			return nil, e, denied and "policy" or "transport"
		end
		capture_turn_state(resp.headers, tkey, tgen)
		local tok, text = pcall(resp.text, resp)
		pcall(resp.close, resp)
		if not tok then
			if req.cancel and req.cancel:is_cancelled() then
				return nil, "cancelled", "cancelled"
			end
			return nil, mare.text.redact(tostring(text)), "transport"
		end
		if not resp.ok then
			local snippet = mare.text.redact(tostring(text):sub(1, 300))
			return nil, ("OpenAI compact %d: %s"):format(resp.status, snippet), compact_http_kind(resp.status)
		end
		local dok, j = pcall(mare.json.decode, text)
		local items = dok and type(j) == "table" and type(j.output) == "table" and M.keep_compacted_items(j.output)
			or {}
		local usage = dok and type(j) == "table" and type(j.usage) == "table" and M.usage(j.usage) or nil
		local has_compaction = false
		for _, item in ipairs(items) do
			if type(item) == "table" and item.type == "compaction" then
				has_compaction = true
				break
			end
		end
		if #items == 0 or not has_compaction then
			return nil, "OpenAI compact returned no usable history", "invalid", usage, codex or nil
		end
		return {
			items = items,
			model = model.id,
			subscription = codex or nil,
			usage = usage,
		}
	end

	llm:register_provider({
		compact = compact,
		warm_url = function()
			return (cfg.base_url or "https://api.openai.com/v1")
		end,
		provider = "openai",
		label = "OpenAI",
		login = login,
		login_label = "ChatGPT subscription or API key",
		-- logging out of openai removes both; credential priority follows
		-- (`init.lua` api_key > ChatGPT / stored login > OPENAI_API_KEY)
		auth_keys = { "openai-codex", "openai" },
		oauth_key = "openai-codex",
		oauth_label = "ChatGPT",
		revoke = revoke,
		hosted_tools = OPENAI_HOSTED_TOOLS,
		model_defaults = {
			["*"] = { edit_tool = "apply_patch", gpt_rules = true },
			["gpt-5.4*"] = { tool_update = "addition_only" },
			["gpt-5.5*"] = { tool_update = "addition_only" },
			["gpt-5.6*"] = { tool_update = "addition_only" },
			["gpt-5.7*"] = { tool_update = "addition_only" },
			["gpt-5.8*"] = { tool_update = "addition_only" },
			["gpt-5.9*"] = { tool_update = "addition_only" },
			["gpt-6*"] = { tool_update = "addition_only" },
		},
		catalog = function(for_fetch)
			if use_oauth() then
				return codex_catalog_descriptor(for_fetch)
			end
			return openai_catalog_descriptor("openai", function()
				return llm:base_url("openai", cfg.base_url or "https://api.openai.com/v1")
			end, api_key, false)(for_fetch)
		end,
		default_model = "gpt-6.1-terra",
		default_context = 400000,
		default_max_output = 128000,
		-- Responses call ids: [A-Za-z0-9_-], at most 64 characters
		tool_id_policy = { max = 64 },
		default_reasoning = true,
		is_configured = function()
			-- ask both: has_key also registers OPENAI_API_KEY as this provider's variable, which
			-- the login status lists as an overridden source (not only when no ChatGPT login is set)
			local oauth_set, key_set = has_oauth(), has_key()
			return oauth_set or key_set
		end,
		-- before a prompt's first request: a ChatGPT token about to expire
		-- is refreshed now. Only a failure that needs a new login refuses the prompt; any
		-- other is temporary (`retryable`): llm.prepare tries it again a little, then the
		-- request goes with the cached token and refreshes itself
		prepare = function(_, opts)
			if not use_oauth() then
				return
			end
			local ok, err = pcall(fresh_oauth, false, opts and opts.cancel)
			if ok then
				return
			elseif type(err) == "table" and err.kind == "auth" then
				error(err, 0)
			end
			local t = type(err) == "table" and err or {}
			error({
				message = type(err) == "table" and tostring(err.message) or tostring(err),
				kind = t.kind or "transport",
				retryable = true,
				retry_after_ms = t.retry_after_ms,
			}, 0)
		end,
		auth_mode = function()
			local cred = llm:active_credential("openai", { "OPENAI_API_KEY" })
			if not cred then
				return nil
			end
			return cred.kind == "oauth" and "chatgpt" or "api_key"
		end,
		catalog_target = function()
			if use_oauth() then
				local o = oauth()
				local wire = codex_request_snapshot(nil, type(o) == "table" and o.account_id or nil)
				return codex_target_from_wire(wire)
			end
			local url =
				mare.http.build_url(llm:base_url("openai", cfg.base_url or "https://api.openai.com/v1") .. "/models")
			return {
				mode = "api_key",
				url = url,
			}
		end,
		catalog_scope = function()
			if use_oauth() then
				return codex_auth_scope(oauth())
			end
			return nil
		end,
		route = "responses",
		wire = "openai-responses",
		endpoint = openai_endpoint,
		stream = stream,
		errors = {
			codes = {
				usage_limit_reached = "usage_limit",
				insufficient_quota = {
					kind = "quota",
					retryable = false,
					hint = "quota exceeded — check your plan and billing",
				},
				usage_not_included = {
					kind = "fatal",
					retryable = false,
					hint = "this model is not included in your plan",
				},
				cyber_policy = "fatal",
				invalid_prompt = "fatal",
				bio_policy = "fatal",
			},
			image_limit = M.ERRORS.image_limit,
			map = function(raw)
				if raw.api_type == "usage_limit_reached" or raw.api_code == "usage_limit_reached" then
					local resets_at = tonumber(raw.resets_at)
					local msg = "usage limit reached" .. (raw.plan_type and (" (" .. raw.plan_type .. " plan)") or "")
					if resets_at then
						msg = msg .. "; resets at " .. os.date("%Y-%m-%d %H:%M", resets_at)
					end
					return { kind = "usage_limit", retryable = false, message = msg }
				end
			end,
		},
		models = {
			-- context_window = measured input limit, not the spec-sheet number
			{
				model_id = "gpt-6.1-terra",
				name = "GPT-6.1 Terra",
				context_window = 921000,
				context_window_measured = true,
				max_output = 128000,
				reasoning = true,
				-- summaries + verbosity supported, token-based output truncation
				reasoning_summary = true,
				verbosity = "low",
				truncation = { mode = "tokens", limit = 10000 },
				efforts = { "off", "low", "medium", "high", "xhigh" },
				tool_update = "addition_only",
				cost = { input = 1, output = 5, cache_read = 0.05, cache_write = 1.25 },
			},
			{
				model_id = "gpt-6.1-sol",
				name = "GPT-6.1 Sol",
				context_window = 500000,
				context_window_measured = true,
				max_output = 128000,
				reasoning = true,
				-- summaries + verbosity supported, token-based output truncation
				reasoning_summary = true,
				verbosity = "low",
				truncation = { mode = "tokens", limit = 10000 },
				efforts = { "low", "medium", "high", "xhigh" },
				tool_update = "addition_only",
				cost = { input = 2, output = 10, cache_read = 0.1, cache_write = 2.5 },
			},
			{
				model_id = "gpt-6-luna",
				name = "GPT-6 Luna",
				context_window = 272000,
				context_window_measured = true,
				max_output = 128000,
				reasoning = true,
				-- summaries + verbosity supported, token-based output truncation
				reasoning_summary = true,
				verbosity = "low",
				truncation = { mode = "tokens", limit = 10000 },
				efforts = { "minimal", "low", "medium", "high" },
				tool_update = "addition_only",
				cost = { input = 0.1, output = 0.5, cache_read = 0.01, cache_write = 0.125 },
			},
			{
				model_id = "gpt-5.6-terra",
				name = "GPT-5.6 Terra",
				context_window = 400000,
				context_window_measured = true,
				max_output = 128000,
				reasoning = true,
				-- summaries + verbosity supported, token-based output truncation
				reasoning_summary = true,
				verbosity = "low",
				truncation = { mode = "tokens", limit = 10000 },
				efforts = { "low", "medium", "high", "xhigh" },
				tool_update = "addition_only",
				cost = { input = 2, output = 12, cache_read = 0.2, cache_write = 2.5 },
			},
			{
				model_id = "gpt-5.6-sol",
				name = "GPT-5.6 Sol",
				context_window = 400000,
				context_window_measured = true,
				max_output = 128000,
				reasoning = true,
				-- summaries + verbosity supported, token-based output truncation
				reasoning_summary = true,
				verbosity = "low",
				truncation = { mode = "tokens", limit = 10000 },
				efforts = { "low", "medium", "high", "xhigh" },
				tool_update = "addition_only",
				cost = { input = 4, output = 20, cache_read = 0.4, cache_write = 5 },
			},
			{
				model_id = "gpt-5.6-luna",
				name = "GPT-5.6 Luna",
				context_window = 400000,
				context_window_measured = true,
				max_output = 128000,
				reasoning = true,
				-- summaries + verbosity supported, token-based output truncation
				reasoning_summary = true,
				verbosity = "low",
				truncation = { mode = "tokens", limit = 10000 },
				efforts = { "minimal", "low", "medium", "high" },
				tool_update = "addition_only",
				cost = { input = 0.2, output = 1.2, cache_read = 0.02, cache_write = 0.25 },
			},
		},
	})
end
