---@meta _
-- Service contract for `workspace`. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, string, boolean = S.object, S.string, S.boolean
return {
	["workspace"] = {
		doc = "Workspace root, directory membership, and project trust controls.",
		methods = {
			get = {
				doc = "Return the canonical workspace root, trust state, and workspace directory list.",
				expose = "both",
				provider = "host",
				access = "peer",
				scope = "workspace",
				params = object({}),
				order = {},
				result = object({
					root = string,
					trusted = boolean,
					dirs = { type = "array", items = string },
				}, { "root", "trusted", "dirs" }),
			},
			set_trust = {
				doc = "Update project trust for this workspace across all active sessions attached to it.",
				expose = "both",
				provider = "host",
				access = "primary",
				admin = true,
				scope = "workspace",
				params = object({
					trusted = boolean,
					save = boolean,
				}, { "trusted" }),
				order = { "trusted", "save" },
				result = object({}),
			},
		},
		events = {
			["changed"] = {
				doc = "Allowed workspace directory roots changed.",
				mode = "emit",
				expose = "lua",
				retain = "last",
				emitters = { "tools", "commands", "session" },
				data = object({ dirs = { type = "array", items = string } }, { "dirs" }),
			},
		},
	},
}
