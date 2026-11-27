---@meta _
-- The `update-check` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, string, boolean = S.object, S.string, S.boolean
local open_object = { type = "object", additionalProperties = true }

local check_result = object({
	latest = string,
	current = string,
	newer = boolean,
}, { "latest", "current", "newer" })

return {
	["update-check"] = {
		doc = "Periodic GitHub release update check.",
		methods = {
			check = {
				doc = "Query the configured release endpoint for a newer mare version; returns `{latest, current, newer}` or `nil, reason`.",
				lua_unchecked = true,
				params = object({}),
				order = {},
				result = { oneOf = { check_result, { type = "null" } } },
				lua_result = { "{latest: string, current: string, newer: boolean}|nil", "string|nil" },
			},
		},
		events = {
			["available"] = {
				doc = "A newer release version was found by update-check.",
				mode = "emit",
				expose = "lua",
				data = open_object,
			},
		},
	},
}
