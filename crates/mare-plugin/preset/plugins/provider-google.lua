-- provider-google: Gemini via the Generative Language API (`google-generate` wire).

local BUDGETS = { minimal = 512, low = 2048, medium = 8192, high = 24576, xhigh = 32768 }
local LEVELS = { minimal = "minimal", low = "low", medium = "medium", high = "high", xhigh = "xhigh" }

return {
	name = "provider-google",
	-- its provider/* hooks go through ctx.bus
	inject = { "llm", "bus" },
	-- a provider: the minimal profile keeps it
	tags = { "provider" },
	providers = { "google" },
	-- the llm wire it registers (llm:register_wire), known even while this plugin is off
	wires = { "google-generate" },
	config_schema = {
		base_url = {
			type = "string",
			default = "https://generativelanguage.googleapis.com/v1beta",
			desc = "API endpoint root.",
		},
		headers = { type = "map", default = {}, desc = "Extra request headers." },
		transport = {
			type = "string",
			default = "auto",
			desc = '"auto", "http" or a transport plugin\'s name (`provider/transport`).',
		},
	},
	apply = function(ctx, cfg)
		local lib = require("mare_lib")
		local llm = ctx.llm

		local function api_key()
			return llm:api_key("google", { "GEMINI_API_KEY", "GOOGLE_API_KEY" })
		end

		-- The wire spec owns model-URL encoding and SSE / plain-JSON decoding; llm.send owns sending.
		-- Defer heavy wire/stream submodules out of apply; each loads on first use.
		local wire_stream
		local ensure_wire = lib.lazy(function()
			local w = require("provider-google.wire")
			w.stream = wire_stream
			return w
		end)
		local ensure_spec = lib.lazy(function()
			return require("provider-google.stream")({ M = ensure_wire() })
		end)
		local wire_errors = setmetatable({}, {
			__index = function(_, k)
				local errs = ensure_spec().errors
				return errs and errs[k] or nil
			end,
		})
		local function decode_google_models(label)
			return function(raw)
				local items, lossless_items = require("mare_lib").decode_models_envelope(raw, "models", "name", label)
				local out = {}
				for i, m in ipairs(items) do
					local id = m.name:gsub("^models/", "")
					if id == "" then
						error(label .. ": invalid response", 0)
					end
					local gen = false
					local lossless_m = lossless_items and lossless_items[i]
					local lossless_methods = lossless_m and lossless_m.supportedGenerationMethods
					if lossless_methods ~= nil then
						if type(lossless_methods) ~= "table" or not mare.json.is_array(lossless_methods) then
							error(label .. ": invalid response", 0)
						end
						for j = 1, #lossless_methods do
							local meth = lossless_methods[j]
							if type(meth) ~= "string" or meth == "" then
								error(label .. ": invalid response", 0)
							end
							gen = gen or meth == "generateContent"
						end
					end
					if gen then
						out[#out + 1] = {
							id = id,
							name = type(m.displayName) == "string" and m.displayName or nil,
							context_window = tonumber(m.inputTokenLimit),
							max_output = tonumber(m.outputTokenLimit),
							reasoning = m.thinking == true or nil,
						}
					end
				end
				return out
			end
		end

		local function google_provider_factory(name, entry)
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

			local function custom_endpoint(_req)
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
				return {
					provider = name,
					label = eff.label or name,
					dialect = eff.dialect,
					base_url = base,
					auth = "x-goog-api-key",
					key = key,
					headers = merged_headers,
					drop_fields = eff.drop_fields,
					transport = eff.transport,
				}
			end

			return {
				route = "google",
				wire = "google-generate",
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
				default_context = 1048576,
				default_max_output = 65536,
				default_reasoning = entry.reasoning ~= false,
				is_configured = function()
					return entry.no_key or raw_key ~= nil or llm:has_api_key(name, envs)
				end,
				errors = wire_errors,
				catalog_target = entry.list_models ~= false and function()
					local eff = live_entry()
					local base = tostring(live_base_url(eff) or ""):gsub("/+$", "")
					return lib.catalog_target(
						ctx.bus,
						name,
						mare.http.build_url(base .. "/models", { pageSize = "1000" }),
						eff.headers
					)
				end or nil,
				catalog_scope = entry.list_models ~= false and function()
					if live_entry().no_key then
						return "no_key"
					end
					return raw_key ~= nil and ("api_key:" .. mare.crypto.sha256(raw_key)) or nil
				end or nil,
				catalog = entry.list_models ~= false and function(for_fetch)
					local eff = live_entry()
					local headers = {}
					for k, v in pairs(eff.headers or {}) do
						headers[k] = v
					end
					local base = tostring(live_base_url(eff) or ""):gsub("/+$", "")
					local label = ("%s GET /models"):format(name)
					return {
						label = label,
						url = base .. "/models?pageSize=1000",
						headers = headers,
						auth = "x-goog-api-key",
						key = for_fetch and custom_api_key(eff) or nil,
						no_key = eff.no_key,
						decode = decode_google_models(label),
					}
				end or nil,
			}
		end

		llm:register_wire({
			name = "google-generate",
			route = "google",
			path = "",
			auth = { "x-goog-api-key", "bearer" },
			errors = wire_errors,
			provider = google_provider_factory,
			model_fields = {
				reasoning_level = "boolean",
			},
			estimate_hints = {
				image_mime_keys = { "mimeType", "mime_type" },
			},
			encode = function(req, ep)
				return ensure_spec().encode(req, ep)
			end,
			decoder = function(sink, context)
				return ensure_spec().decoder(sink, context)
			end,
		})
		wire_stream = llm:wire("google-generate")

		local function endpoint(_req)
			local key = api_key()
			if not key then
				error({
					message = "Google is not configured: run /login google or set GEMINI_API_KEY",
					retryable = false,
				})
			end
			return {
				provider = "google",
				label = "Google",
				base_url = llm:base_url("google", cfg.base_url),
				key = key,
				headers = cfg.headers,
				transport = cfg.transport,
			}
		end

		local function stream(req, sink)
			return wire_stream(endpoint, req, sink)
		end

		-- the Google route's effort mappings, today's constants
		-- as data: thinkingLevel models (unknown level → high; off sends nothing) and budget
		-- models (thinkingBudget, unknown level → 8192; off = budget 0)
		local TC = "generationConfig.thinkingConfig."
		local leveled = { { value = "off", wire = { google = {} } } }
		local budgeted = { { value = "off", wire = { google = { [TC .. "thinkingBudget"] = 0 } } } }
		for _, e in ipairs({ "minimal", "low", "medium", "high", "xhigh" }) do
			leveled[#leveled + 1] = {
				value = e,
				wire = { google = { [TC .. "includeThoughts"] = true, [TC .. "thinkingLevel"] = LEVELS[e] } },
			}
			budgeted[#budgeted + 1] = {
				value = e,
				wire = { google = { [TC .. "includeThoughts"] = true, [TC .. "thinkingBudget"] = BUDGETS[e] } },
			}
		end
		llm:define_params({
			route = "google",
			when = function(m)
				return (m --[[@as table]]).reasoning_level
			end,
			define = {
				effort = {
					options = leveled,
					wire = { google = { [TC .. "includeThoughts"] = true, [TC .. "thinkingLevel"] = "high" } },
				},
			},
		})
		llm:define_params({
			route = "google",
			when = function(m)
				return not (m --[[@as table]]).reasoning_level
			end,
			define = {
				effort = {
					options = budgeted,
					wire = { google = { [TC .. "includeThoughts"] = true, [TC .. "thinkingBudget"] = 8192 } },
				},
			},
		})

		local efforts = { "off", "minimal", "low", "medium", "high", "xhigh" }
		llm:register_provider({
			warm_url = function()
				return (cfg.base_url or "https://generativelanguage.googleapis.com")
			end,
			provider = "google",
			label = "Google",
			wire = "google-generate",
			key_prefixes = { "AIza" },
			endpoint = endpoint,
			login = {
				methods = { "api_key" },
				run = function(lio)
					llm:login_api_key("google", "Gemini API key", lio)
				end,
			},
			catalog_target = function()
				return {
					url = mare.http.build_url(llm:base_url("google", cfg.base_url) .. "/models", { pageSize = "1000" }),
				}
			end,
			-- GET {base}/models: declarative catalog fetched via llm catalog.fetch
			catalog = function(for_fetch)
				return {
					label = "Google GET /models",
					url = llm:base_url("google", cfg.base_url) .. "/models?pageSize=1000",
					auth = "x-goog-api-key",
					key = for_fetch and api_key() or nil,
					decode = decode_google_models("Google GET /models"),
				}
			end,
			login_label = "API key (aistudio.google.com)",
			default_model = "gemini-2.5-pro",
			default_context = 1048576,
			default_max_output = 65536,
			default_reasoning = true,
			is_configured = function()
				return llm:has_api_key("google", { "GEMINI_API_KEY", "GOOGLE_API_KEY" })
			end,
			route = "google",
			stream = stream,
			models = {
				{
					model_id = "gemini-3-pro-preview",
					name = "Gemini 3 Pro",
					context_window = 1048576,
					max_output = 65536,
					reasoning = true,
					reasoning_level = true,
					efforts = { "low", "high" },
					cost = { input = 2, output = 12, cache_read = 0.2 },
				},
				{
					model_id = "gemini-2.5-pro",
					name = "Gemini 2.5 Pro",
					context_window = 1048576,
					max_output = 65536,
					reasoning = true,
					efforts = { "minimal", "low", "medium", "high", "xhigh" },
					cost = { input = 1.25, output = 10, cache_read = 0.125 },
				},
				{
					model_id = "gemini-2.5-flash",
					name = "Gemini 2.5 Flash",
					context_window = 1048576,
					max_output = 65536,
					reasoning = true,
					efforts = efforts,
					cost = { input = 0.3, output = 2.5, cache_read = 0.03 },
				},
			},
		})
	end,
}
