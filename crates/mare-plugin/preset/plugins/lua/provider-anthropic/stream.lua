-- Anthropic Messages wire: encode requests and decode events. Sending and classification live
-- in llm.send; compatibility callbacks preserve existing wire behavior.
return function(D)
	local cfg, M = D.cfg, D.M

	local function encode(req, endpoint)
		local cfg = endpoint.wire_config
		local model = req.model
		local max_tokens = req.max_tokens or model.max_output or cfg.default_max_output
		if model.max_output and max_tokens > model.max_output then
			max_tokens = model.max_output
		end
		local schema_sanitize = require("mare_lib.schema_sanitize")
		local dialect =
			schema_sanitize.detect_dialect(endpoint.dialect, endpoint.base_url, model.id, { route = "anthropic" })
		local tools = M.convert_tools(req.tools, dialect)
		local deferred_tools = {}
		local has_deferred = false
		for _, t in ipairs(tools) do
			if t.defer_loading then
				deferred_tools[t.name] = true
				has_deferred = true
			end
		end
		local lib = require("mare_lib")
		local prov_cfg = endpoint.provider_config
		local route_cfg = (endpoint.route_cache_retention ~= nil or endpoint.route_supports_long_cache_retention ~= nil)
				and {
					cache_retention = endpoint.route_cache_retention,
					supports_long_cache_retention = endpoint.route_supports_long_cache_retention,
				}
			or nil
		local plugin_cfg = endpoint.provider == "anthropic" and cfg
			or (
				(endpoint.plugin_cache_retention ~= nil or endpoint.plugin_supports_long_cache_retention ~= nil)
					and {
						cache_retention = endpoint.plugin_cache_retention,
						supports_long_cache_retention = endpoint.plugin_supports_long_cache_retention,
					}
				or nil
			)
		local retention, read_only, supports_long =
			lib.resolve_cache_retention(req, model, prov_cfg, plugin_cfg, route_cfg, endpoint.global_config)
		local req_native = type(req.native) == "table"
				and type(req.native["anthropic-messages"]) == "table"
				and req.native["anthropic-messages"]
			or nil
		local cache_control = (req_native and type(req_native.cache_control) == "table" and req_native.cache_control)
			or lib.anthropic_cache_control(retention, supports_long, read_only)
		local system_cc = (req.system and req.system ~= "" and cache_control ~= nil) and 1 or 0
		local msg_state = {}
		local body = {
			model = model.id,
			max_tokens = max_tokens,
			stream = true,
			messages = M.convert_messages(req.messages, cache_control == nil, {
				provider = endpoint.provider,
				replay_unsigned = endpoint.replay_unsigned_reasoning,
				deferred_tools = has_deferred and deferred_tools or nil,
				cache_control = cache_control,
				max_cache_breakpoints = 4 - system_cc,
				state = msg_state,
			}),
		}
		if req.system and req.system ~= "" then
			-- req.cache == false / "read" or cache_retention == "none": don't write the prompt cache
			body.system = {
				{
					type = "text",
					text = req.system,
					cache_control = cache_control and { type = cache_control.type, ttl = cache_control.ttl } or nil,
				},
			}
		end
		if #tools > 0 then
			body.tools = tools
			if req.tool_choice == "none" then
				body.tool_choice = { type = "none" } -- summary requests
			end
		end
		if req.output_schema ~= nil then
			local out_dialect = schema_sanitize.detect_dialect(
				endpoint.dialect,
				endpoint.base_url,
				model.id,
				{ route = "anthropic", output_schema = true }
			)
			body.output_config = body.output_config or {}
			body.output_config.format = {
				type = "json_schema",
				schema = schema_sanitize.sanitize(req.output_schema, out_dialect),
			}
		end
		local headers = {
			["anthropic-version"] = M.VERSION,
			["accept"] = "text/event-stream",
		}
		return {
			method = "POST",
			url = endpoint.url,
			headers = headers,
			body = body,
		}, {
			has_deferred = has_deferred,
			tools = tools,
			invalidated_current_turn = msg_state.invalidated_current_turn == true,
		}
	end

	local function finalize(req, endpoint, wire, state)
		local headers, body, model = wire.headers, wire.body, req.model
		local has_deferred, tools = state.has_deferred, state.tools
		if state.invalidated_current_turn then
			-- A signed thinking block in the active tool turn was modified (e.g. secret projection
			-- or lossy UTF-8 replacement) and omitted to avoid replaying a stale signature.
			-- Explicitly disable thinking rather than deleting `body.thinking` (some models keep
			-- thinking enabled when the parameter is omitted); if the endpoint still rejects a
			-- thinking-less active tool turn, `retry` surfaces a recoverable error instead of
			-- silently stripping `body.thinking`.
			body.thinking = { type = "disabled" }
			wire._invalidated_current_turn = true
		end
		local function add_beta(tag)
			local beta_key
			for k in pairs(headers) do
				if type(k) == "string" and string.lower(k) == "anthropic-beta" then
					beta_key = k
					break
				end
			end
			beta_key = beta_key or "anthropic-beta"
			local cur = headers[beta_key]
			if type(cur) == "string" and cur ~= "" then
				if not cur:find(tag, 1, true) then
					headers[beta_key] = cur .. "," .. tag
				end
			else
				headers[beta_key] = tag
			end
		end
		if has_deferred then
			add_beta("advanced-tool-use-2025-11-20")
		end
		if #tools > 0 and endpoint.fine_grained_tool_streaming ~= false then
			add_beta("fine-grained-tool-streaming-2025-05-14")
		end
		if not model.adaptive and type(body.thinking) == "table" and body.thinking.type == "enabled" then
			add_beta("interleaved-thinking-2025-05-14")
		end
		local req_native = type(req.native) == "table"
				and type(req.native["anthropic-messages"]) == "table"
				and req.native["anthropic-messages"]
			or nil
		if req_native and type(req_native.betas) == "table" then
			for _, tag in ipairs(req_native.betas) do
				if type(tag) == "string" and tag ~= "" then
					add_beta(tag)
				end
			end
		end
	end

	local function extract(payload)
		if type(payload) ~= "table" or type(payload.error) ~= "table" then
			return nil
		end
		local api = payload.error
		return {
			api_code = api.code,
			api_type = api.type,
			message = api.message,
			status = api.status,
			request_id = api.request_id,
			trace_id = api.trace_id,
		}
	end

	local function retry(wire, err)
		if err.status ~= 400 and err.status ~= 422 then
			return false
		end
		local msg = type(err.message) == "string" and err.message:lower() or ""
		if wire._invalidated_current_turn then
			if
				msg:find("thinking", 1, true)
				or msg:find("redacted_thinking", 1, true)
				or msg:find("signature", 1, true)
			then
				error({
					message = "Anthropic: active tool turn thinking block was modified and cannot be replayed safely ("
						.. tostring(err.message or "endpoint rejected continuation without original thinking block")
						.. "); send a new user message to continue on a fresh turn boundary",
					kind = "invalid",
					retryable = false,
				}, 0)
			end
			return false
		end
		if type(wire.body.thinking) ~= "table" or wire.body.thinking.type ~= "disabled" then
			return false
		end
		if
			msg:find("thinking", 1, true)
			and (
				msg:find("disabled", 1, true)
				or msg:find("not supported", 1, true)
				or msg:find("enabled", 1, true)
				or msg:find("between_tools", 1, true)
			)
		then
			wire.body.thinking = msg:find("between_tools", 1, true) and { type = "between_tools" } or nil
			return true
		end
		return false
	end

	local function filter_tools(tools, req, meta)
		local schema_sanitize = require("mare_lib.schema_sanitize")
		local model = (req and req.model) or (meta and meta.model)
		local dialect = schema_sanitize.detect_dialect(
			meta and meta.dialect,
			meta and meta.base_url,
			model and model.id,
			{ route = "anthropic" }
		)
		return M.filter_tools(tools, dialect)
	end

	return {
		name = "anthropic-messages",
		config = cfg,
		route = "anthropic",
		path = "/v1/messages",
		auth = { "x-api-key", "bearer" },
		encode = encode,
		filter_tools = filter_tools,
		finalize = finalize,
		error = extract,
		retry = retry,
		errors = {
			codes = M.ERROR_KINDS,
			overflow = M.OVERFLOW,
			image_limit = M.IMAGE_LIMIT,
		},
		estimate_hints = {
			image_mime_keys = { "media_type" },
		},
		decoder = function(sink, context)
			local endpoint, wire = context.endpoint, context.request
			local parser = M.new_parser(sink, function(api)
				return { source = "stream", payload = api }
			end, {
				is_openrouter = require("mare_lib").is_openrouter(endpoint.dialect, wire.url),
			})
			return { event = parser.feed, finish = parser.finish }
		end,
	}
end
