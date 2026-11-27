---@meta _
-- The `query` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, integer, string, boolean, list_of = S.object, S.integer, S.string, S.boolean, S.list_of
local query_provider = object({
	name = string,
	revision = string,
	priority = integer,
	operations = { type = "array", items = string },
	source = object({
		capability = { type = "string", enum = { "in_process", "opaque" } },
	}, { "capability" }),
}, { "name", "revision", "priority", "operations", "source" })
return {
	["query"] = {
		doc = "Capability-based read-only query registry and operation dispatcher.",
		errors = {
			stale_cursor = { doc = "the cursor came from another operation, provider or generation; restart query" },
			provider_changed = { doc = "the provider was replaced or removed while the query ran" },
		},
		methods = {
			register = {
				doc = "Register capability operations owned by this caller; removed automatically on disposal. "
					.. "Paginated results return next_cursor; subsequent requests receive cursor.",
				expose = "lua",
				registration = true,
				lua = {
					name = "string",
					def = '{priority?: integer, source?: {capability: "in_process"|"opaque"}, operations: table<string, fun(args: table, opts?: {cancel?: mare.Token, provider?: string}): table>}',
				},
				order = { "name", "def" },
			},
			list_providers = {
				doc = "Registered query providers and the operations they serve, sorted by name.",
				expose = "both",
				access = "peer",
				params = object({}),
				order = {},
				result = list_of(query_provider),
			},
			supports = {
				doc = "Whether a registered in-process provider serves this operation.",
				expose = "lua",
				params = object({ operation = string }, { "operation" }),
				order = { "operation" },
				result = boolean,
			},
			call = {
				doc = "Run an in-process query operation inside an ephemeral-read-only sandbox "
					.. "rooted at the active execution context's cwd, or at `mare.cwd` when called directly "
					.. "from Lua without an active `mare.exec.current()`. Lua callers may pass `cancel`, "
					.. "a function returning their cancel token or nil.",
				expose = "both",
				access = "peer",
				params = object({
					operation = string,
					arguments = { type = "object", additionalProperties = true },
					provider = string,
				}, { "operation", "arguments" }),
				order = { "operation", "arguments", "..." },
				callbacks = { cancel = "fun(): mare.Token|nil" },
				errors = { "stale_cursor", "provider_changed" },
				result = {
					type = "object",
					properties = { provider = string, next_cursor = string },
					additionalProperties = true,
				},
			},
		},
		events = {
			["changed"] = {
				doc = "Registered query providers or their operations changed.",
				mode = "emit",
				expose = "lua",
				retain = "last",
			},
		},
	},
}
