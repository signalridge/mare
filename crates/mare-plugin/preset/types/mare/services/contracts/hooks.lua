---@meta _
-- The `hooks` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, string, boolean = S.object, S.string, S.boolean
local hook_mode = {
	type = "string",
	enum = { "emit", "parallel", "serial", "waterfall", "policy" },
}
return {
	["hooks"] = {
		doc = "Subscribe to Cordis bus events and policy decision points from plugins or RPC clients.",
		methods = {
			subscribe = {
				doc = "Subscribe `handle(event, mode, args)` to a bus event or policy decision point. "
					.. "Returns a registration removed on release or client disconnect. "
					.. "`prepend = true` inserts before earlier listeners (`prepend = false` keeps append order); "
					.. "`timeout_ms` requires `>= 1` and is clamped to the host policy timeout ceiling for remote callers.",
				expose = "both",
				access = "primary",
				admin = true,
				params = object({
					event = string,
					prepend = boolean,
					timeout_ms = { type = "integer", minimum = 1 },
				}, { "event" }),
				order = { "event", "..." },
				registration = true,
				require_explicit_scope = true,
				release_on = "never",
				callbacks = {
					handle = {
						type = "fun(event: string, mode: string, args: any[]): any",
						params = object({
							event = string,
							mode = hook_mode,
							args = { type = "array" },
						}, { "event", "mode", "args" }),
						order = { "event", "mode", "args" },
					},
				},
			},
		},
	},
}
