---@meta _
-- The `message` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, integer, string, boolean, list_of = S.object, S.integer, S.string, S.boolean, S.list_of
local type_descriptor = object({ type = string }, { "type" })
local delivery_spec = object({
	trigger_turn = boolean,
	deliver_as = { type = "string", enum = { "steer", "follow_up", "next_turn" } },
	wait = boolean,
})
return {
	["message"] = {
		doc = "Custom message types and delivery-aware custom messages entering model context.",
		methods = {
			register_type = {
				doc = "Register a custom message type with optional validation and render callbacks; returns a disposer (`registration_id` over RPC).",
				expose = "both",
				access = "peer",
				scope = "host",
				registration = true,
				params = object({ type = string }, { "type" }),
				order = { "type", "..." },
				callbacks = {
					validate = {
						type = "fun(msg: mare.CustomMessageInput): boolean|nil, string|nil",
						params = object({ message = { type = "object" } }, { "message" }),
						order = { "message" },
						result = object({ valid = boolean, reason = string }, { "valid" }),
						optional = true,
					},
					render = {
						type = "fun(msg: table, opts: {expanded: boolean}): mare.Node|nil",
						params = object({
							message = { type = "object" },
							opts = object({ expanded = boolean }, { "expanded" }),
						}, { "message", "opts" }),
						order = { "message", "opts" },
						result = object({ node = {} }),
						optional = true,
					},
					compact = "fun(msg: table, opts: {expanded: boolean}): mare.Node|nil",
					expanded = "fun(msg: table, opts: {expanded: boolean}): mare.Node|nil",
				},
			},
			list_types = {
				doc = "List currently registered custom message types in sorted name order.",
				expose = "both",
				access = "peer",
				scope = "host",
				params = object({}),
				order = {},
				result = list_of(type_descriptor),
			},
			append = {
				doc = "Append a custom message that enters model context while honoring display, delivery, and optional `request_id` idempotency.",
				expose = "both",
				access = "peer",
				scope = "session",
				params = object({
					type = string,
					content = { type = { "string", "array" } },
					display = boolean,
					details = {},
					created_at_ms = integer,
					delivery = delivery_spec,
					request_id = string,
				}, { "type", "content" }),
				order = { "..." },
				result = object({
					status = { type = "string", enum = { "appended", "queued", "deferred", "triggered" } },
					message = { type = "object" },
					result = { type = "object" },
					duplicate = boolean,
				}, { "status", "message" }),
			},
			get_type = {
				doc = "Look up the normalized active spec for a registered custom message type (Lua-only).",
				expose = "lua",
				lua_unchecked = true,
				params = object({ custom_type = string }, { "custom_type" }),
				order = { "custom_type" },
				result = { oneOf = { { type = "object", additionalProperties = true }, { type = "null" } } },
				lua_result = "mare.CustomMessageSpec?",
			},
		},
	},
}
