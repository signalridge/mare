---@meta _
-- The `usage` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, integer, number, string, usage_totals, boolean, cost, rate_limit =
	S.object, S.integer, S.number, S.string, S.usage_totals, S.boolean, S.cost, S.rate_limit
return {
	["usage"] = {
		doc = "Token and cost accounting of the session.",
		events = {
			["changed"] = {
				doc = "Session token usage and cost totals changed.",
				mode = "emit",
				expose = "both",
				delivery = "lossy",
				ignorable = false,
				retain = "last",
				emitters = { "compact" },
				data = usage_totals,
				example = {
					input = 1.0,
					output = 2.0,
					cache_read = 0.0,
					cache_write = 0.0,
					cost = 0.01,
					requests = 1.0,
					subscription_requests = 0.0,
					side_requests = 0.0,
					last_context = 10.0,
					last_cache_read = 0.0,
				},
			},
		},
		methods = {
			get = {
				doc = "Totals, context tokens and rate limits of the session in one object.",
				expose = "both",
				access = "peer",
				scope = "session",
				result = object({
					totals = usage_totals,
					context_tokens = integer,
					rate_limits = { type = "object", additionalProperties = rate_limit },
				}, { "totals", "context_tokens", "rate_limits" }),
			},
			-- Lua-only, with wire schemas so a remote provider can implement the whole contract.
			totals = {
				doc = "The session's totals: reset by a new session, recounted on resume.",
				result = usage_totals,
			},
			context_tokens = { doc = "Tokens occupying the context window after the last request.", result = integer },
			set_context = {
				doc = "Update the context token estimate and emit usage/changed; notify = false is silent.",
				params = object({ tokens = integer, notify = boolean }, { "tokens" }),
				order = { "tokens", "..." },
				result = object({}),
			},
			cost_text = {
				doc = 'Cost as text: "$0.1234", "subscription, not billed", or both.',
				params = object({ format = string }),
				order = { "format" },
				result = string,
			},
			rate_limits = {
				doc = "Latest subscription quota per provider (the llm/rate-limits event).",
				result = { type = "object", additionalProperties = rate_limit },
			},
			quota_lines = {
				doc = "Subscription quota as display lines, one per window (/status, the statusline).",
				result = {
					type = "array",
					items = object({ text = string, warning = boolean, gauge = number }, { "text" }),
				},
			},
		},
	},
}
