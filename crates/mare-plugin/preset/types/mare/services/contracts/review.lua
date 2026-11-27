---@meta _
-- The `review` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, string = S.object, S.string
local open_object = { type = "object", additionalProperties = true }

return {
	["review"] = {
		doc = "Read-only automated code review runner and interactive change diff viewer.",
		methods = {
			run = {
				doc = "Run an isolated read-only code review for `target` and return findings or `nil, err`.",
				lua_unchecked = true,
				params = object({ target = open_object }),
				order = { "target" },
				result = { oneOf = { open_object, { type = "null" } } },
				lua_result = { "table|nil", "string|nil" },
			},
			viewer = {
				doc = "Open the interactive change diff viewer, optionally narrowed to `only`.",
				lua_unchecked = true,
				params = object({ only = string }),
				order = { "only" },
				result = { type = { "object", "null" } },
			},
		},
		events = {
			["completed"] = {
				doc = "Automated code review run completed.",
				mode = "emit",
				expose = "lua",
				data = object({
					target = open_object,
					hint = string,
					text = string,
					error = string,
				}, { "text" }, true),
			},
		},
	},
}
