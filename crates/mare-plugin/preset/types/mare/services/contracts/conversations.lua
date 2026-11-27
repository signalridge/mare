---@meta _
-- The `conversations` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, string, integer, boolean = S.object, S.string, S.integer, S.boolean
local open_object = { type = "object", additionalProperties = true }

local parent_status = object({
	state = {
		type = "string",
		enum = { "idle", "working", "needs_approval", "needs_input", "interrupted", "failed", "closed", "finished" },
	},
	detail = string,
	actionable = boolean,
}, { "state" })

local conversation_info = object({
	conversation_id = string,
	session_id = string,
	selected = boolean,
	busy = boolean,
	model = string,
	params = open_object,
	message_count = integer,
	parent_status = parent_status,
	turn_id = string,
	ui_context_id = string,
}, { "conversation_id", "selected", "busy", "message_count" }, true)

return {
	["conversations"] = {
		doc = "Ephemeral, parent-isolated side conversations branched from a committed session snapshot.",
		methods = {
			start = {
				doc = "Start a side conversation from a committed snapshot of the current session.",
				expose = "lua",
				lua_unchecked = true,
				params = object({
					before_item_id = string,
					model = string,
					effort = string,
				}),
				order = { "..." },
				result = { oneOf = { conversation_info, { type = "null" } } },
				lua_result = { "mare.ConversationInfo|nil", "string?" },
			},
			info = {
				doc = "Return the open side conversation snapshot, or nil when closed.",
				expose = "lua",
				poisoned = "allow",
				lua_unchecked = true,
				params = object({}),
				order = {},
				result = { oneOf = { conversation_info, { type = "null" } } },
				lua_result = "mare.ConversationInfo|nil",
			},
			close = {
				doc = "Cancel and discard the active side conversation.",
				expose = "lua",
				lua_unchecked = true,
				params = object({}),
				order = {},
				result = { type = { "boolean", "null" } },
				lua_result = { "boolean|nil", "string?" },
			},
			cancel = {
				doc = "Cancel the active side conversation turn without closing it.",
				expose = "lua",
				params = object({}),
				order = {},
				result = boolean,
			},
			toggle = {
				doc = "Toggle focus between the main conversation and an open side conversation.",
				expose = "lua",
				lua_unchecked = true,
				params = object({}),
				order = {},
				result = { oneOf = { conversation_info, { type = "null" } } },
				lua_result = { "mare.ConversationInfo|nil", "string?" },
			},
		},
	},
}
