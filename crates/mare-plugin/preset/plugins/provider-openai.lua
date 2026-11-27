-- provider-openai: Responses API. Two auth modes sharing one request format:
--   * ChatGPT subscription (Codex OAuth)  → https://chatgpt.com/backend-api/codex/responses
--   * API key (OPENAI_API_KEY or /login) → https://api.openai.com/v1/responses

local CLIENT_ID = "app_EMoamEEZ73f0CkXaXp7hrann"
local AUTH_BASE = "https://auth.openai.com"
local REDIRECT_URI = "http://localhost:1455/auth/callback"

return {
	name = "provider-openai",
	-- its provider/* hooks and llm/rate-limits go through ctx.bus
	inject = { "llm", "bus" },
	-- a provider: the minimal profile keeps it
	tags = { "provider" },
	-- the llm wire it registers (llm:register_wire), known even while this plugin is off
	wires = { "openai-responses" },
	providers = { "openai" },
	config_schema = {
		base_url = { type = "string", desc = "API-key endpoint override (default https://api.openai.com/v1)." },
		codex_base_url = {
			type = "string",
			default = "https://chatgpt.com/backend-api",
			desc = "ChatGPT (Codex) endpoint root.",
		},
		auth_base_url = { type = "string", default = AUTH_BASE, desc = "Auth endpoint root." },
		verbosity = { type = "string", desc = "Overrides the model default (low|medium|high)." },
		stream_idle_timeout_ms = {
			type = "integer",
			default = 300000,
			desc = "No bytes for this long ends a stream as a transport error.",
		},
		originator = { type = "string", default = "mare", desc = "Originator reported to the API." },
		headers = {
			type = "map",
			default = {},
			desc = "Extra request headers, merged over the computed ones (e.g. a proxy's auth header).",
		},
		-- "auto" | "websocket" | "http" | any name a transport plugin answers on
		-- `provider/transport`. auto: a Responses WebSocket (one long connection reused across
		-- turns) on the official OpenAI / ChatGPT endpoints, HTTP
		-- streaming elsewhere (custom base URLs, gateways) and whenever a proxy is configured;
		-- a WebSocket that can't connect falls back to HTTP for the rest of the session
		transport = {
			type = "string",
			default = "auto",
			desc = '"auto", "websocket", "http" or a transport plugin\'s name (`provider/transport`).',
		},
		websocket_connect_timeout_ms = { type = "integer", default = 15000, desc = "WebSocket connect timeout." },
		compact_timeout_ms = { type = "integer", desc = "Timeout for /responses/compact requests (ms)." },
		callback_ports = {
			type = "list",
			items = { type = "integer" },
			desc = "Local ports tried for the browser login callback (default 1455, 1457).",
		},
		device_code_timeout_ms = {
			type = "integer",
			desc = "How long the device-code login waits (default 15 minutes).",
		},
		reasoning_summary = { type = "string", desc = 'Reasoning summary mode sent with requests (default "auto").' },
		session_header = {
			type = "string",
			desc = "Request header carrying the session id on every request of a session; nil = none.",
		},
		-- OpenAI hosted web_search tool:
		-- nil / "disabled" = off (explicit opt-in);
		-- "cached" | "indexed" | "live" = declare `{ type = "web_search", ... }` on OpenAI requests.
		web_search = {
			type = "enum",
			choices = { "disabled", "cached", "indexed", "live" },
			desc = 'OpenAI hosted web_search tool; nil / "disabled" = off (explicit opt-in).',
		},
		-- Optional policy/config passthrough: { filters = { allowed_domains = ... },
		-- user_location = { type = "approximate", country?, region?, city?, timezone? },
		-- search_context_size = "low"|"medium"|"high", search_content_types = { "text", "image" } }
		web_search_config = { type = "map", desc = "Policy / config passthrough of the web_search tool." },
		cache_retention = {
			type = "enum",
			choices = { "none", "short", "long" },
			desc = 'Prompt cache retention tier: "none" | "short" | "long" (nil = global/short).',
		},
	},
	apply = function(ctx, cfg)
		require("mare_lib").validate_cache_retention(cfg.cache_retention, "provider-openai")
		if cfg.web_search ~= nil then
			assert(
				cfg.web_search == "disabled"
					or cfg.web_search == "cached"
					or cfg.web_search == "indexed"
					or cfg.web_search == "live",
				"provider-openai: web_search must be disabled, cached, indexed or live"
			)
		end
		if cfg.web_search_config ~= nil then
			assert(type(cfg.web_search_config) == "table", "provider-openai: web_search_config must be a table")
			local scs = cfg.web_search_config.search_context_size
			if scs ~= nil then
				assert(
					scs == "low" or scs == "medium" or scs == "high",
					"provider-openai: web_search_config.search_context_size must be low, medium or high"
				)
			end
		end
		-- Defer heavy wire/auth/login/websocket submodules out of apply; each loads on first use.
		local lib = require("mare_lib")
		local LOGIN_METHODS = { "oauth", "device", "api_key" }
		local function is_ws_target(ev)
			if type(ev) ~= "table" then
				return false
			end
			local raw_ev = ev --[[@as table]]
			local ep = type(raw_ev.endpoint) == "table" and raw_ev.endpoint or nil
			local req = type(raw_ev.request) == "table" and raw_ev.request or nil
			local url = req and req.url or nil
			if type(url) ~= "string" or not url:match("^https?://") then
				return false
			end
			return ev.provider == "openai"
				or (ep ~= nil and (ep.websocket == true or ep.transport == "websocket"))
				or ev.transport == "websocket"
		end
		local ensure_wire = lib.lazy(function()
			return require("provider-openai.wire")
		end)
		local M = setmetatable({
			ERRORS = require("provider-openai.errors"),
		}, {
			__index = function(_, k)
				return ensure_wire()[k]
			end,
		})
		local D = {
			ctx = ctx,
			cfg = cfg,
			M = M,
			CLIENT_ID = CLIENT_ID,
			AUTH_BASE = AUTH_BASE,
			LOGIN_METHODS = LOGIN_METHODS,
			is_ws_target = is_ws_target,
		}
		local ensure_auth = lib.lazy(function()
			return require("provider-openai.auth")(D)
		end)
		D.auth = {
			llm = ctx.llm,
			oauth = function()
				return ensure_auth().oauth()
			end,
			has_oauth = function()
				return ensure_auth().has_oauth()
			end,
			api_key = function()
				return ensure_auth().api_key()
			end,
			has_key = function()
				return ensure_auth().has_key()
			end,
			token_request = function(...)
				return ensure_auth().token_request(...)
			end,
			fresh_oauth = function(...)
				return ensure_auth().fresh_oauth(...)
			end,
		}
		local ensure_login = lib.lazy(function()
			return require("provider-openai.login")(D)
		end)
		D.login = {
			spec = {
				methods = LOGIN_METHODS,
				run = function(lio)
					return ensure_login().spec.run(lio)
				end,
			},
			login = function(lio)
				return ensure_login().login(lio)
			end,
			revoke = function(key, entry)
				return ensure_login().revoke(key, entry)
			end,
		}
		local ensure_websocket = lib.lazy(function()
			return require("provider-openai.websocket")(D)
		end)
		ctx:on("provider/transport", function(ev)
			if not is_ws_target(ev) then
				return nil
			end
			return ensure_websocket().handle_transport(ev)
		end)
		require("provider-openai.requests")(D)
	end,
}
