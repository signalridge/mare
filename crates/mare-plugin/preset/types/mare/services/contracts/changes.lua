---@meta _
-- The `changes` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, string, integer, boolean = S.object, S.string, S.integer, S.boolean
local open_object = { type = "object", additionalProperties = true }

local change_file = object({
	path = string,
	before = { oneOf = { string, boolean } },
	after = { oneOf = { string, boolean } },
	missing = boolean,
	old = string,
	new = string,
	adds = integer,
	dels = integer,
}, { "path" }, true)

local change_turn = object({
	turn = string,
	label = string,
	shell = boolean,
	files = { type = "array", items = change_file },
}, { "turn", "label", "shell", "files" }, true)

return {
	["changes"] = {
		doc = "Per-turn workspace file change journal, diffs, and rewind.",
		methods = {
			turns = {
				doc = "Recorded journal turns with modified files, oldest first.",
				lua_unchecked = true,
				params = object({}),
				order = {},
				result = { type = "array", items = change_turn },
				lua_result = "mare.ChangeTurn[]",
			},
			diffs = {
				doc = "Each turn's before/after file contents and diff stats, oldest first.",
				lua_unchecked = true,
				params = object({}),
				order = {},
				result = { type = "array", items = change_turn },
				lua_result = "mare.ChangeTurn[]",
			},
			rewind = {
				doc = "Restore workspace files to their snapshot before `turn`.",
				lua_unchecked = true,
				params = object({
					turn = string,
					force = boolean,
				}, { "turn" }),
				order = { "turn", "..." },
				result = { oneOf = { open_object, { type = "null" } } },
			},
		},
		events = {
			["rewound"] = {
				doc = "Workspace file changes were rewound to an earlier turn.",
				mode = "emit",
				expose = "lua",
				data = object({
					turn = string,
					restored = { type = "array", items = string },
					conflicts = { type = "array", items = string },
					missing = { type = "array", items = string },
					refused = { type = "array", items = string },
					failed = { type = "array", items = open_object },
				}, { "turn" }, true),
			},
		},
	},
}
