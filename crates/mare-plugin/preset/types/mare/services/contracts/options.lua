---@meta _
-- The `options` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, string, boolean = S.object, S.string, S.boolean
return {
	["options"] = {
		doc = "Host-provided configuration read and transactional update service.",
		methods = {
			get = {
				doc = "Read effective options (or descriptors when opts.describe is true) with live service overrides and shared sensitive-path redaction.",
				expose = "lua",
				poisoned = "allow",
				params = object({
					keys = { type = "array", items = string },
					describe = boolean,
				}),
				order = { "keys", "..." },
				result = { type = "object", additionalProperties = true },
			},
			set = {
				doc = "Validate and transactionally commit a batch of option updates (core options, llm model/effort, and plugins.<plugin>.<option>) with full rollback on refusal.",
				expose = "lua",
				poisoned = "deny",
				params = object({
					values = { type = "object", additionalProperties = true },
					validate_only = boolean,
				}, { "values" }),
				order = { "values", "..." },
				result = object({}),
			},
		},
	},
}
