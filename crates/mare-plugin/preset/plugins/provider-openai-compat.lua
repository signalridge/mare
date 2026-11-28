-- provider-openai-compat: Chat Completions wire (`openai-chat`) and shipped presets.
-- Custom providers are defined via `mare.provider(name, { base_url = ..., ... })` and materialized
-- through the `openai-chat` wire's `provider` factory (`lua/provider-openai-compat/endpoint.lua`).

local EFFORTS = { "low", "medium", "high" }

local PRESETS = {
	deepseek = {
		label = "DeepSeek",
		wire = "openai-chat",
		dialect = "deepseek",
		models_dev = "deepseek",
		base_url = "https://api.deepseek.com",
		api_key_env = "DEEPSEEK_API_KEY",
		needs_reasoning_placeholder = true,
		default_model = "deepseek-chat",
		models = {
			{
				model_id = "deepseek-chat",
				name = "DeepSeek V3.2",
				context_window = 128000,
				max_output = 8192,
				cost = { input = 0.28, output = 0.42, cache_read = 0.028 },
			},
			{
				model_id = "deepseek-reasoner",
				-- DeepSeek API docs: deepseek-reasoner is always-on thinking mode and does
				-- not support disabling thinking (`efforts = { "high" }` omits `"off"`).
				name = "DeepSeek V3.2 (thinking; always on)",
				context_window = 128000,
				max_output = 64000,
				reasoning = true,
				efforts = { "high" },
				cost = { input = 0.28, output = 0.42, cache_read = 0.028 },
			},
		},
	},
	kimi = {
		label = "Kimi",
		wire = "openai-chat",
		dialect = "kimi",
		models_dev = "moonshotai",
		base_url = "https://api.moonshot.ai/v1",
		api_key_env = "MOONSHOT_API_KEY",
		default_model = "kimi-k2-0905-preview",
		models = {
			{
				model_id = "kimi-k2-0905-preview",
				name = "Kimi K2",
				context_window = 256000,
				max_output = 32768,
				cost = { input = 0.6, output = 2.5, cache_read = 0.15 },
			},
			{
				model_id = "kimi-k2-thinking",
				name = "Kimi K2 Thinking",
				context_window = 256000,
				max_output = 32768,
				reasoning = true,
				cost = { input = 0.6, output = 2.5, cache_read = 0.15 },
			},
			{
				model_id = "kimi-k3",
				name = "Kimi K3",
				context_window = 262144,
				max_output = 131072,
				reasoning = true,
				-- Moonshot Kimi K3 docs: always-on thinking, top-level reasoning_effort = low | high | max.
				efforts = { "low", "high", "max" },
				tool_update = "addition_only",
				cost = { input = 3, output = 15, cache_read = 0.3 },
			},
		},
	},
	["kimi-code"] = {
		label = "Kimi Code",
		wire = "openai-chat",
		dialect = "kimi",
		models_dev = "kimi-for-coding",
		base_url = "https://api.kimi.com/coding/v1",
		api_key_env = "KIMI_API_KEY",
		default_model = "kimi-for-coding",
		models = {
			{
				model_id = "kimi-for-coding",
				name = "Kimi for Coding",
				context_window = 262144,
				max_output = 32768,
				reasoning = true,
			},
		},
	},
	openrouter = {
		label = "OpenRouter",
		wire = "openai-chat",
		dialect = "openrouter",
		models_dev = "openrouter",
		base_url = "https://openrouter.ai/api/v1",
		api_key_env = "OPENROUTER_API_KEY",
		key_prefixes = { "sk-or-" },
		effort_param = "openrouter",
		default_model = "anthropic/claude-sonnet-5-5",
		models = {
			{
				model_id = "anthropic/claude-sonnet-5-5",
				name = "Claude Sonnet 5.5 (OpenRouter)",
				context_window = 200000,
				max_output = 64000,
				reasoning = true,
				efforts = EFFORTS,
				cost = { input = 3, output = 15 },
			},
			{
				model_id = "google/gemini-2.5-pro",
				name = "Gemini 2.5 Pro (OpenRouter)",
				context_window = 1048576,
				max_output = 65536,
				reasoning = true,
				efforts = EFFORTS,
				cost = { input = 1.25, output = 10 },
			},
			{
				model_id = "deepseek/deepseek-chat",
				name = "DeepSeek (OpenRouter)",
				context_window = 128000,
				max_output = 8192,
				cost = { input = 0.28, output = 0.42 },
			},
		},
	},
	ollama = {
		label = "Ollama",
		wire = "openai-chat",
		base_url = "http://localhost:11434/v1",
		no_key = true,
		models = {},
	},
	lmstudio = {
		label = "LM Studio",
		wire = "openai-chat",
		base_url = "http://localhost:1234/v1",
		no_key = true,
		models = {},
	},
}

local PRESET_PROVIDERS = {}
for name in pairs(PRESETS) do
	PRESET_PROVIDERS[#PRESET_PROVIDERS + 1] = name
end
table.sort(PRESET_PROVIDERS)

local PROVIDER_FIELDS = {
	effort_param = "enum:openrouter|reasoning_effort",
	reasoning_format = "enum:openrouter|reasoning_effort|deepseek|kimi|zai|together|qwen|qwen-chat-template|chat-template",
	reasoning_budget_field = "string",
	reasoning_budgets = "table",
	supports_reasoning_effort = "boolean",
	cache_control_format = "string",
	supports_long_cache_retention = "boolean",
	supports_prompt_cache_key = "boolean",
	needs_reasoning_placeholder = "boolean",
	markup_healing = "string",
}

local MODEL_FIELDS = {
	needs_reasoning_placeholder = "boolean",
	supports_long_cache_retention = "boolean",
	supports_prompt_cache_key = "boolean",
	cache_control_format = "string",
	markup_healing = "string",
}

return {
	name = "provider-openai-compat",
	-- its provider/* hooks go through ctx.bus
	inject = { "llm", "bus" },
	-- a provider: the minimal profile keeps it
	tags = { "provider" },
	wires = { "openai-chat" },
	providers = PRESET_PROVIDERS,
	config_schema = {
		headers = {
			type = "map",
			default = {},
			desc = "Extra request headers for every endpoint; per-endpoint `headers` win.",
		},
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
		lib.validate_cache_retention(cfg.cache_retention, "provider-openai-compat")
		local llm = ctx.llm
		local ensure_wire = lib.lazy(function()
			return require("provider-openai-compat.wire")
		end)
		local wire_errors = setmetatable({}, {
			__index = function(_, k)
				local w = ensure_wire()
				if k == "codes" then
					return w.COMPAT_CODES
				elseif k == "overflow" then
					return w.OVERFLOW
				elseif k == "image_limit" then
					return w.IMAGE_LIMIT
				end
			end,
		})
		local endpoint_factory =
			require("provider-openai-compat.endpoint")({ llm = llm, cfg = cfg, wire = ensure_wire })

		llm:register_wire({
			name = "openai-chat",
			config = cfg,
			route = "chat",
			path = "/chat/completions",
			auth = { "bearer" },
			errors = wire_errors,
			provider = endpoint_factory,
			provider_fields = PROVIDER_FIELDS,
			model_fields = MODEL_FIELDS,
			enrich_model = function(m, entry, mark)
				if type(entry) ~= "table" then
					return
				end
				if m.needs_reasoning_placeholder == nil then
					local il = entry.interleaved
					if il == "reasoning_content" or (type(il) == "table" and il.field == "reasoning_content") then
						m.needs_reasoning_placeholder = true
						mark("needs_reasoning_placeholder")
					end
				end
				if m.tool_update == nil and entry.dynamically_loaded_tools == true then
					m.tool_update = "addition_only"
					mark("tool_update")
				end
			end,
			encode = function(req, ep)
				local wire = ensure_wire()
				local prov_name = ep.provider or ep.name or "openai-compat"
				local model = req.model
				local schema_sanitize = require("mare_lib.schema_sanitize")
				local compat_base = ep.base_url
					or (type(ep.url) == "string" and ep.url:gsub("/chat/completions$", "") or "")
				local wire_url = ep.url
				local dialect = schema_sanitize.detect_dialect(ep.dialect, compat_base, model.id)
				local kimi_defer = dialect.kimi == true and model.tool_update == "addition_only"
				local tools, deferred_tools =
					wire.convert_tools(req.tools, dialect, kimi_defer and { defer_loading = true } or nil)
				local has_tools = type(req.tools) == "table" and #req.tools > 0
				local prov_cfg = ep.provider_config
				local ep_cfg = type(ep.endpoint_config) == "table" and ep.endpoint_config or ep
				local retention, read_only, supports_long =
					lib.resolve_cache_retention(req, model, prov_cfg, ep.wire_config or cfg, ep_cfg, ep.global_config)
				local placeholder_reasoning = lib.needs_reasoning_placeholder(
					model,
					ep,
					dialect.deepseek == true
						or (type(wire_url) == "string" and wire_url:lower():find("deepseek", 1, true) ~= nil),
					llm:model_sources(model.ref)
				)
				local healing_pattern = wire.resolve_markup_healing(ep, model, compat_base)
				local body = {
					model = model.id,
					messages = wire.convert_messages(req.system, req.messages, prov_name, {
						url = wire_url,
						model_id = model.id,
						has_tools = has_tools,
						placeholder_reasoning = placeholder_reasoning,
						deferred_tools = deferred_tools,
						markup_healing = healing_pattern,
					}),
					stream = true,
					stream_options = { include_usage = true },
				}
				local is_openai_url = wire.is_openai_host(wire_url)
				local explicit_long = lib.resolve_cache_cap(
					"supports_long_cache_retention",
					nil,
					model,
					prov_cfg,
					ep_cfg,
					ep.wire_config or cfg
				)
				local allow_long = supports_long and (is_openai_url or explicit_long == true)
				local explicit_key = lib.resolve_cache_cap(
					"supports_prompt_cache_key",
					nil,
					model,
					prov_cfg,
					ep_cfg,
					ep.wire_config or cfg
				)
				local allow_key
				if explicit_key ~= nil then
					allow_key = explicit_key == true
				else
					allow_key = is_openai_url or (retention == "long" and allow_long)
				end
				-- the one prompt_cache_key decision: retention "none"
				-- (req.cache = false included) and an explicit supports_prompt_cache_key = false omit it
				if retention ~= "none" and allow_key then
					body.prompt_cache_key = wire.clamp_prompt_cache_key(req.session_id)
				end
				if retention == "long" and not read_only and allow_long then
					body.prompt_cache_retention = "24h"
				end
				local cc_format =
					lib.resolve_cache_cap("cache_control_format", nil, model, prov_cfg, ep_cfg, ep.wire_config or cfg)
				local is_openrouter = ep.dialect == "openrouter"
					or ep_cfg.effort_param == "openrouter"
					or (type(prov_cfg) == "table" and prov_cfg.effort_param == "openrouter")
					or lib.is_openrouter(ep.dialect, compat_base)
				local use_anthropic_cc = cc_format == "anthropic"
					or (is_openrouter and type(model.id) == "string" and model.id:match("^anthropic/") ~= nil)
				if #tools > 0 then
					body.tools = tools
					if req.tool_choice == "none" then
						body.tool_choice = "none" -- summary requests
					end
				end
				if use_anthropic_cc then
					local cc = lib.anthropic_cache_control(retention, supports_long, read_only)
					if cc ~= nil then
						wire.apply_anthropic_cache_control(body.messages, body.tools, cc)
					end
				end
				if req.max_tokens then
					body.max_tokens = req.max_tokens
				end
				if req.output_schema ~= nil then
					local out_dialect =
						schema_sanitize.detect_dialect(ep.dialect, compat_base, model.id, { output_schema = true })
					body.response_format = {
						type = "json_schema",
						json_schema = {
							name = "mare_output_schema",
							strict = true,
							schema = schema_sanitize.sanitize(req.output_schema, out_dialect),
						},
					}
				end
				local headers = { accept = "text/event-stream" }
				-- OpenRouter session affinity for sticky routing
				if is_openrouter and type(req.session_id) == "string" and req.session_id ~= "" then
					headers["x-session-id"] = req.session_id
				end
				return {
					method = "POST",
					url = wire_url,
					headers = headers,
					body = body,
					model = model.id,
				}, { markup_healing = healing_pattern }
			end,
			decoder = function(sink, context)
				local wire = ensure_wire()
				local ep, wire_req = context.endpoint or {}, context.request or {}
				local st = type(context.state) == "table" and context.state or {}
				local healing_pattern = st.markup_healing
				if healing_pattern == nil then
					healing_pattern = wire.resolve_markup_healing(ep, wire_req.model, ep.base_url or wire_req.url)
				end
				local P = wire.new_parser(sink, function(e)
					return { source = "stream", payload = e }
				end, {
					is_openrouter = lib.is_openrouter(ep.dialect, wire_req.url),
					markup_healing = healing_pattern,
				})
				return { event = P.feed, finish = P.finish_message }
			end,
		})

		local batch = {}
		for _, name in ipairs(PRESET_PROVIDERS) do
			local entry = table.copy(PRESETS[name])
			entry.provider = name
			batch[#batch + 1] = entry
		end
		llm:register_providers(batch)
	end,
}
