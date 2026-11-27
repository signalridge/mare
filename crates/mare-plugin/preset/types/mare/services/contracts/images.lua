---@meta _
-- The `images` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object = S.object
local open_object = { type = "object", additionalProperties = true }

return {
	["images"] = {
		doc = "Inline terminal image rendering for transcript message and tool-result blocks.",
		methods = {
			show = {
				doc = "Render image blocks from a message or tool-result content list when an image protocol is active.",
				lua_unchecked = true,
				params = object({
					content = { type = "array", items = open_object },
				}, { "content" }),
				order = { "content" },
				result = { type = { "object", "null" } },
			},
		},
	},
}
