-- provider-anthropic: Messages API with API key.

-- budget-based thinking (older models)
local BUDGETS = { minimal = 1024, low = 4096, medium = 10240, high = 24576, xhigh = 32768 }
-- adaptive thinking effort (newer models)
local ADAPTIVE = { minimal = "low", low = "low", medium = "medium", high = "high", xhigh = "max" }

return {
	name = "provider-anthropic",
	-- its provider/* hooks go through ctx.bus
	inject = { "llm", "bus" },
	-- a provider: the minimal profile keeps it
	tags = { "provider" },
	-- the llm wire it registers (llm:register_wire), known even while this plugin is off
	wires = { "anthropic-messages" },
	providers = { "anthropic" },
	config_schema = {
		base_url = { type = "string", default = "https://api.anthropic.com", desc = "API endpoint root." },
		-- reasoning budget per effort (non-adaptive models) and the room kept for the answer
		reasoning_budgets = {
			type = "map",
			default = BUDGETS,
			desc = "Reasoning budget per effort for non-adaptive models.",
		},
		reasoning_display = {
			type = "string",
			default = "summarized",
			desc = 'How reasoning is returned: "summarized" or "omitted".',
		},
		answer_headroom = {
			type = "integer",
			default = 8192,
			desc = "Room kept for the answer next to the reasoning budget.",
		},
		default_max_output = { type = "integer", default = 32000, desc = "Output cap when the model declares none." },
		headers = { type = "map", default = {}, desc = "Extra request headers." },
		transport = {
			type = "string",
			default = "auto",
			desc = '"auto", "http" or a transport plugin\'s name (`provider/transport`).',
		},
		cache_retention = {
			type = "enum",
			choices = { "none", "short", "long" },
			desc = 'Prompt cache retention tier: "none" | "short" | "long" (nil = global/short).',
		},
	},
	apply = function(ctx, cfg)
		local lib = require("mare_lib")
		lib.validate_cache_retention(cfg.cache_retention, "provider-anthropic")
		local llm = ctx.llm

		local function api_key()
			return llm:api_key("anthropic", { "ANTHROPIC_API_KEY" })
		end

		-- The spec owns Messages encoding and decoding; llm owns the send path.
		-- Defer heavy wire/stream submodules out of apply; each loads on first use.
		local wire_stream
		local ensure_wire = lib.lazy(function()
			local w = require("provider-anthropic.wire")
			w.stream = wire_stream
			return w
		end)
		local ensure_spec = lib.lazy(function()
			return require("provider-anthropic.stream")({ cfg = cfg, M = ensure_wire() })
		end)
		local wire_errors = setmetatable({}, {
			__index = function(_, k)
				local errs = ensure_spec().errors
				return errs and errs[k] or nil
			end,
		})
		local function anthropic_v1_url(base, leaf)
			local trimmed = tostring(base or ""):gsub("/+$", "")
			if trimmed:sub(-3) == "/v1" then
				return trimmed .. "/" .. leaf
			end
			return trimmed .. "/v1/" .. leaf
		end

		local function decode_anthropic_models(label)
			return function(raw)
				local items, _, decoded = require("mare_lib").decode_models_envelope(raw, "data", "id", label)
				if type(decoded) == "table" and decoded.type == "error" then
					error(label .. ": invalid response")
				end
				local out = {}
				for _, m in ipairs(items) do
					out[#out + 1] = {
						id = m.id,
						name = type(m.display_name) == "string" and m.display_name or nil,
						context_window = tonumber(m.max_input_tokens),
						max_output = tonumber(m.max_tokens),
					}
				end
				return out
			end
		end

		local function build_anthropic_param_rules(provider_scope, budgets_cfg, display_cfg, headroom)
			local display = (display_cfg == "summarized" or display_cfg == "omitted") and display_cfg or nil
			local adaptive = { { value = "off", wire = { anthropic = {} } } }
			for _, e in ipairs({ "minimal", "low", "medium", "high", "xhigh" }) do
				local wire = { ["thinking.type"] = "adaptive", ["output_config.effort"] = ADAPTIVE[e] }
				if display then
					wire["thinking.display"] = display
				end
				adaptive[#adaptive + 1] = {
					value = e,
					wire = { anthropic = wire },
				}
			end
			local default_adaptive = { ["thinking.type"] = "adaptive", ["output_config.effort"] = "high" }
			if display then
				default_adaptive["thinking.display"] = display
			end
			return {
				{
					provider = provider_scope,
					route = "anthropic",
					when = function(m)
						return (m --[[@as table]]).adaptive
					end,
					define = {
						effort = {
							options = adaptive,
							wire = { anthropic = default_adaptive },
						},
					},
				},
				{
					provider = provider_scope,
					route = "anthropic",
					when = function(m)
						return not (m --[[@as table]]).adaptive
					end,
					define = {
						effort = {
							options = { { value = "off", wire = { anthropic = {} } } },
							wire = {
								anthropic = function(req, c)
									local budgets = budgets_cfg or BUDGETS
									local budget = budgets[c.value] or budgets.medium or BUDGETS.medium
									local max_out = c.model and c.model.max_output
									if max_out and req.body.max_tokens > max_out then
										req.body.max_tokens = max_out
									end
									if req.body.max_tokens <= budget then
										local desired = budget + headroom
										if max_out and desired > max_out then
											desired = max_out
										end
										req.body.max_tokens = desired
									end
									if max_out and req.body.max_tokens > max_out then
										req.body.max_tokens = max_out
									end
									if req.body.max_tokens <= 1024 then
										req.body.thinking = nil
										return
									end
									if budget >= req.body.max_tokens then
										budget = math.max(1024, req.body.max_tokens - math.min(headroom or 1024, 1024))
										if budget >= req.body.max_tokens then
											budget = req.body.max_tokens - 1
										end
									end
									local th = { type = "enabled", budget_tokens = budget }
									if display then
										th.display = display
									end
									req.body.thinking = th
								end,
							},
						},
					},
				},
			}
		end

		local function anthropic_provider_factory(name, entry)
			local envs = (type(entry.api_key_env) == "string" and entry.api_key_env ~= "") and { entry.api_key_env }
				or {}
			local cfg_has_key = type(((mare.config.providers or {})[name] or {}).api_key) ~= "nil"
			local raw_key = not cfg_has_key
					and type(entry.api_key) == "string"
					and entry.api_key ~= ""
					and entry.api_key
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

			local function custom_api_key(eff)
				if eff and eff.no_key then
					return nil
				end
				return raw_key or llm:api_key(name, envs)
			end

			local function custom_endpoint(req)
				local eff = live_entry()
				local key = custom_api_key(eff)
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
				local merged_headers = {}
				for k, v in pairs(eff.headers or {}) do
					merged_headers[k] = v
				end
				local base = live_base_url(eff)
				local model = type(req) == "table" and req.model or nil
				local replay_unsigned
				if model and model.replay_unsigned_reasoning ~= nil then
					replay_unsigned = model.replay_unsigned_reasoning
				else
					replay_unsigned = eff.replay_unsigned_reasoning == true
				end
				return {
					provider = name,
					label = eff.label or name,
					dialect = eff.dialect,
					base_url = base,
					url = anthropic_v1_url(base, "messages"),
					auth = eff.auth or "x-api-key",
					key = key,
					headers = merged_headers,
					drop_fields = eff.drop_fields,
					transport = eff.transport,
					replay_unsigned_reasoning = replay_unsigned,
					fine_grained_tool_streaming = eff.fine_grained_tool_streaming,
				}
			end

			local param_rules
			if entry.reasoning_budgets ~= nil or entry.reasoning_display ~= nil then
				param_rules = build_anthropic_param_rules(
					name,
					entry.reasoning_budgets or cfg.reasoning_budgets or BUDGETS,
					entry.reasoning_display ~= nil and entry.reasoning_display or cfg.reasoning_display,
					cfg.answer_headroom
				)
			end

			return {
				route = "anthropic",
				wire = "anthropic-messages",
				endpoint = custom_endpoint,
				warm_url = function()
					return live_base_url(live_entry())
				end,
				stream = function(req, sink)
					return wire_stream(custom_endpoint, req, sink)
				end,
				no_key = entry.no_key or nil,
				login = not entry.no_key and {
					methods = { "api_key" },
					run = function(lio)
						llm:login_api_key(name, nil, lio)
					end,
				} or nil,
				login_label = "API key",
				default_context = 200000,
				default_max_output = cfg.default_max_output,
				default_reasoning = entry.reasoning ~= false,
				tool_id_policy = { max = 64 },
				is_configured = function()
					return entry.no_key or raw_key ~= nil or llm:has_api_key(name, envs)
				end,
				errors = wire_errors,
				params = param_rules,
				catalog_target = entry.list_models ~= false and function()
					local eff = live_entry()
					return {
						url = mare.http.build_url(anthropic_v1_url(live_base_url(eff), "models"), { limit = "1000" }),
						headers = eff.headers,
						query_params = eff.query_params,
					}
				end or nil,
				catalog_scope = entry.list_models ~= false and function()
					if live_entry().no_key then
						return "no_key"
					end
					return raw_key ~= nil and ("api_key:" .. mare.crypto.sha256(raw_key)) or nil
				end or nil,
				catalog = entry.list_models ~= false and function(for_fetch)
					local eff = live_entry()
					local headers = { ["anthropic-version"] = ensure_wire().VERSION }
					for k, v in pairs(eff.headers or {}) do
						headers[k] = v
					end
					local label = ("%s GET /v1/models"):format(name)
					return {
						label = label,
						url = anthropic_v1_url(live_base_url(eff), "models") .. "?limit=1000",
						headers = headers,
						auth = eff.auth or "x-api-key",
						key = for_fetch and custom_api_key(eff) or nil,
						no_key = eff.no_key,
						decode = decode_anthropic_models(label),
					}
				end or nil,
			}
		end

		-- Register generic Anthropic route parameter rules before `register_wire`, because
		-- `register_wire` synchronously materializes configured data providers whose
		-- provider-scoped rules must register later and take precedence.
		for _, rule in
			ipairs(
				build_anthropic_param_rules(
					nil,
					cfg.reasoning_budgets or BUDGETS,
					cfg.reasoning_display,
					cfg.answer_headroom
				)
			)
		do
			llm:define_params(rule)
		end

		llm:register_wire({
			name = "anthropic-messages",
			config = cfg,
			route = "anthropic",
			path = "/v1/messages",
			auth = { "x-api-key", "bearer" },
			encode = function(req, ep)
				return ensure_spec().encode(req, ep)
			end,
			finalize = function(req, ep, wire, state)
				return ensure_spec().finalize(req, ep, wire, state)
			end,
			error = function(payload)
				return ensure_spec().error(payload)
			end,
			retry = function(wire, err)
				return ensure_spec().retry(wire, err)
			end,
			errors = wire_errors,
			provider = anthropic_provider_factory,
			provider_fields = {
				auth = "string",
				replay_unsigned_reasoning = "boolean",
				fine_grained_tool_streaming = "boolean",
				supports_long_cache_retention = "boolean",
				reasoning_budgets = "table",
				reasoning_display = "string",
			},
			model_fields = {
				adaptive = "boolean",
				replay_unsigned_reasoning = "boolean",
				supports_long_cache_retention = "boolean",
			},
			estimate_hints = {
				image_mime_keys = { "media_type" },
			},
			decoder = function(sink, context)
				return ensure_spec().decoder(sink, context)
			end,
		})
		wire_stream = llm:wire("anthropic-messages")

		local function endpoint(_req)
			local key = api_key()
			if not key then
				error({
					message = "Anthropic is not configured: run /login anthropic or set ANTHROPIC_API_KEY",
					retryable = false,
				})
			end
			return {
				provider = "anthropic",
				label = "Anthropic",
				url = llm:base_url("anthropic", cfg.base_url) .. "/v1/messages",
				key = key,
				headers = cfg.headers,
				transport = cfg.transport,
			}
		end

		local function stream(req, sink)
			return wire_stream(endpoint, req, sink)
		end

		local efforts = { "off", "low", "medium", "high", "xhigh" }
		llm:register_provider({
			warm_url = function()
				return (cfg.base_url or "https://api.anthropic.com")
			end,
			provider = "anthropic",
			label = "Anthropic",
			wire = "anthropic-messages",
			key_prefixes = { "sk-ant-" },
			endpoint = endpoint,
			login = {
				methods = { "api_key" },
				run = function(lio)
					llm:login_api_key("anthropic", "Anthropic API key", lio)
				end,
			},
			catalog_target = function()
				return {
					url = mare.http.build_url(
						llm:base_url("anthropic", cfg.base_url) .. "/v1/models",
						{ limit = "1000" }
					),
				}
			end,
			-- GET /v1/models; declarative catalog fetched via llm catalog.fetch
			catalog = function(for_fetch)
				return {
					label = "Anthropic GET /v1/models",
					url = llm:base_url("anthropic", cfg.base_url) .. "/v1/models?limit=1000",
					headers = { ["anthropic-version"] = ensure_wire().VERSION },
					auth = "x-api-key",
					key = for_fetch and api_key() or nil,
					decode = decode_anthropic_models("Anthropic GET /v1/models"),
				}
			end,
			login_label = "API key (console.anthropic.com)",
			default_model = "claude-sonnet-5-5",
			default_context = 200000,
			default_max_output = 32000,
			default_reasoning = true,
			-- Anthropic: ^[a-zA-Z0-9_-]+$, at most 64 characters
			tool_id_policy = { max = 64 },
			is_configured = function()
				return llm:has_api_key("anthropic", { "ANTHROPIC_API_KEY" })
			end,
			route = "anthropic",
			tool_update = "addition_only",
			aliases = {
				sonnet = { family = "sonnet" },
				opus = { family = "opus" },
				haiku = { family = "haiku" },
			},
			stream = stream,
			models = {
				{
					model_id = "claude-opus-5-5",
					name = "Claude Opus 5.5",
					context_window = 200000,
					max_output = 64000,
					reasoning = true,
					adaptive = true,
					efforts = efforts,
					cost = { input = 5, output = 25, cache_read = 0.5, cache_write = 6.25 },
				},
				{
					model_id = "claude-sonnet-5-5",
					name = "Claude Sonnet 5.5",
					context_window = 200000,
					max_output = 64000,
					reasoning = true,
					adaptive = true,
					efforts = efforts,
					cost = { input = 3, output = 15, cache_read = 0.3, cache_write = 3.75 },
				},
				{
					model_id = "claude-haiku-5-5",
					name = "Claude Haiku 5.5",
					context_window = 200000,
					max_output = 64000,
					reasoning = true,
					adaptive = true,
					efforts = efforts,
					cost = { input = 1, output = 5, cache_read = 0.1, cache_write = 1.25 },
				},
				{
					model_id = "claude-fable-5-1",
					name = "Claude Fable 5.1",
					context_window = 200000,
					max_output = 64000,
					reasoning = true,
					adaptive = true,
					efforts = efforts,
					cost = { input = 3, output = 15, cache_read = 0.3, cache_write = 3.75 },
				},
				{
					model_id = "claude-sonnet-4-5",
					name = "Claude Sonnet 4.5",
					context_window = 200000,
					max_output = 64000,
					reasoning = true,
					efforts = { "off", "minimal", "low", "medium", "high", "xhigh" },
					cost = { input = 3, output = 15, cache_read = 0.3, cache_write = 3.75 },
				},
			},
		})
	end,
}
