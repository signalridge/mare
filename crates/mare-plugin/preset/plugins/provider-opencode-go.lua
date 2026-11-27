-- provider-opencode-go: the opencode Go subscription, one gateway with a route per
-- model. The gateway serves a model over several wire formats; which one a model takes is a
-- property of the model (a provider-level protocol would override every model). This plugin owns
-- the shared parts — gateway address, API key, the `x-opencode-session` header on every route,
-- the catalogue — and hands each request to the wire of the model's route: `responses` →
-- "openai-responses", `anthropic` → "anthropic-messages" (llm wire registry; whichever plugin
-- registers them, its `describe` giving the path and its definition's `wires` the plugin to
-- blame). The chat route stays off.
--
--   mare.plugin("provider-opencode-go", { model_routes = { ["glm-*"] = "anthropic" } })
--   mare.model { provider = "opencode-go", id = "glm-5.3-flash", route = "anthropic" }

return {
	name = "provider-opencode-go",
	-- it registers on the llm service and answers `doctor`
	inject = { "llm" },
	-- a provider: the minimal profile keeps it
	tags = { "provider" },
	providers = { "opencode-go" },
	config_schema = {
		base_url = {
			type = "string",
			default = "https://opencode.ai/inference/go",
			desc = "The gateway root; each route's address is base_url .. path.",
		},
		-- route → { path, wire (llm wire name), auth, replay_unsigned_reasoning, drop_fields }. A
		-- route's address is base_url .. path .. the wire's own path (its `describe`); a wire
		-- that isn't there is blamed on the plugin declaring it (`wires = { … }`)
		routes = {
			type = "map",
			desc = "Gateway routes by name: path, llm wire, auth and per-route wire options.",
			default = {
				responses = { path = "/openai/v1", wire = "openai-responses" },
				-- Bearer auth like the Responses route (sends Authorization, no
				-- x-api-key); the gateway returns glm's thinking unsigned: replayed within the
				-- current tool loop
				anthropic = {
					path = "/anthropic",
					wire = "anthropic-messages",
					auth = "bearer",
					replay_unsigned_reasoning = true,
				},
				-- models.dev lists glm / deepseek on Chat Completions; no chat wire is registered yet
				-- chat = { path = "/openai/v1", wire = "openai-chat" },
			},
		},
		default_route = {
			type = "string",
			default = "responses",
			desc = "Route of every model unless something below picks another.",
		},
		model_routes = {
			type = "map",
			default = {},
			desc = "Glob over model ids (or provider/id) to route; the longest pattern wins.",
		},
		models_dev_routes = {
			type = "boolean",
			default = false,
			desc = "Take a listed model's route from its models.dev protocol (only onto configured routes).",
		},
		session_header = {
			type = "string",
			default = "x-opencode-session",
			desc = "Stable per-session header on every request, main and auxiliary (400 MissingSessionID without it).",
		},
		custom_tools = {
			type = "boolean",
			default = false,
			desc = "Send `custom` (freeform) tools; the gateway rejects them with a 400.",
		},
		cache_retention = {
			type = "enum",
			choices = { "none", "short", "long" },
			desc = 'Prompt cache retention tier: "none" | "short" | "long" (nil = global/short).',
		},
		supports_long_cache_retention = {
			type = "boolean",
			desc = "Whether the provider supports long (1h / 24h / 12h) prompt cache retention.",
		},
	},
	apply = function(ctx, cfg)
		local lib = require("mare_lib")
		lib.validate_cache_retention(cfg.cache_retention, "provider-opencode-go")
		for rname, rc in pairs(type(cfg.routes) == "table" and cfg.routes or {}) do
			if type(rc) == "table" then
				lib.validate_cache_retention(rc.cache_retention, "provider-opencode-go.routes." .. tostring(rname))
			end
		end
		require("provider-opencode-go.provider")({ ctx = ctx, cfg = cfg, llm = ctx.llm })
	end,
}
