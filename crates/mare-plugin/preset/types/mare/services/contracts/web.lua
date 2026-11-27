---@meta _
-- The `web` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, integer, string, boolean, list_of = S.object, S.integer, S.string, S.boolean, S.list_of
local web_kind = { type = "string", enum = { "search", "fetch" } }
local web_provider = object({
	provider_id = string,
	available = boolean,
	unconfined = boolean,
	selected = boolean,
}, { "provider_id", "available", "unconfined", "selected" })
local web_source = object({
	url = string,
	title = string,
	summary = string,
	snippet = string,
	published_at_ms = integer,
}, { "url" })
local web_search_result = list_of(web_source, {
	content = string,
	truncated = boolean,
})
web_search_result.title = "mare.WebSearchResult"
local web_fetch_result = object({
	requested_url = string,
	url = string,
	status = integer,
	content_type = string,
	content = string,
	truncated = boolean,
}, { "requested_url", "content" })
web_fetch_result.title = "mare.WebFetchResult"
return {
	["web"] = {
		doc = "Web search and URL retrieval service backed by registered providers.",
		errors = {},
		events = {
			["providers-changed"] = {
				doc = "Registered web search or fetch providers changed.",
				mode = "emit",
				expose = "both",
				scope = "host",
				delivery = "lossy",
				ignorable = false,
				retain = "last",
				data = object({ kind = string, provider_id = string, action = string }, { "kind" }, true),
				example = { kind = "search", provider_id = "brave", action = "added" },
			},
		},
		methods = {
			register_search = {
				doc = "Register a search backend owned by the caller; removed on release or disconnect.",
				expose = "both",
				access = "peer",
				admin = true,
				registration = true,
				params = object({
					provider_id = { type = "string", minLength = 1, maxLength = 64 },
					unconfined = boolean,
				}, { "provider_id" }),
				order = { "..." },
				callbacks = {
					search = {
						type = "fun(request: table, ctx?: mare.WebCallContext): mare.WebSearchResult",
						params = object({ request = { type = "object", additionalProperties = true } }, { "request" }),
						order = { "request" },
						result = web_search_result,
					},
					available = "fun(model?: table): boolean",
				},
			},
			register_fetch = {
				doc = "Register a fetch backend owned by the caller; removed on release or disconnect.",
				expose = "both",
				access = "peer",
				admin = true,
				registration = true,
				params = object({
					provider_id = { type = "string", minLength = 1, maxLength = 64 },
					unconfined = boolean,
				}, { "provider_id" }),
				order = { "..." },
				callbacks = {
					fetch = {
						type = "fun(request: table, ctx?: mare.WebCallContext): mare.WebFetchResult",
						params = object({ request = { type = "object", additionalProperties = true } }, { "request" }),
						order = { "request" },
						result = web_fetch_result,
					},
					available = "fun(model?: table): boolean",
				},
			},
			list_providers = {
				doc = "Registered web backends of `kind` sorted by provider_id, each marked `selected` when active.",
				expose = "both",
				access = "peer",
				params = object({ kind = web_kind }, { "kind" }),
				order = { "kind" },
				result = list_of(web_provider, { reason = string }),
			},
			search = {
				doc = "Search the web using the currently selected search backend under active sandbox limits.",
				expose = "both",
				access = "peer",
				confined = true,
				params = object({
					queries = { type = "array", items = string },
					timeout_ms = integer,
				}, { "queries" }),
				order = { "queries", "..." },
				callbacks = { cancel = "fun(): mare.Token|nil" },
				result = web_search_result,
			},
			fetch = {
				doc = "Fetch a URL using the currently selected fetch backend under active sandbox limits.",
				expose = "both",
				access = "peer",
				confined = true,
				params = object({
					url = string,
					timeout_ms = integer,
				}, { "url" }),
				order = { "url", "..." },
				callbacks = { cancel = "fun(): mare.Token|nil" },
				result = web_fetch_result,
			},
		},
	},
}
