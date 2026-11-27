---@meta _
-- The `commands` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, string, boolean, list_of = S.object, S.string, S.boolean, S.list_of

local availability_kind = { type = "string", enum = { "always", "idle", "session", "main", "running", "tui" } }
local availability = {
	oneOf = {
		availability_kind,
		{ type = "array", items = availability_kind },
	},
}

local command_info_props = {
	name = string,
	description = string,
	args = string,
	auto_execute = boolean,
	local_only = boolean,
	availability = availability,
	alias_of = string,
}
local command_info = object(command_info_props, { "name" })
local command_info_or_nil = {
	type = { "object", "null" },
	properties = command_info_props,
	required = { "name" },
	additionalProperties = false,
}

local command_input = object({
	content = {
		type = "array",
		items = object({
			type = string,
			text = string,
			data = string,
			mime = string,
			url = string,
			path = string,
			name = string,
		}, { "type" }, true),
	},
}, nil, true)

local completion_item = object({
	label = string,
	insert = string,
	detail = string,
	icon = string,
	style = string,
}, { "label" }, true)

return {
	["commands"] = {
		doc = "Slash-command registry and dispatch.",
		errors = {
			unknown_command = { doc = "no registered command has that name, or its alias target is missing" },
			command_unavailable = { doc = "the command's availability condition does not hold right now" },
			command_failed = { doc = "the command failed or returned a failure outcome" },
			local_only = { doc = "the command is local to the terminal UI and cannot run remotely" },
		},
		methods = {
			list = {
				doc = "Registered non-alias slash commands, sorted by name (data only, no callbacks).",
				expose = "both",
				access = "peer",
				result = list_of(command_info),
			},
			get = {
				doc = "Current definition data of command `name` (or alias), or nil if absent.",
				expose = "both",
				access = "peer",
				params = object({ name = string }, { "name" }),
				order = { "name" },
				result = command_info_or_nil,
			},
			run = {
				doc = "Run a slash command line (optionally targeting a loaded `session_id`); returns {result?} on success and throws a coded error on failure.",
				expose = "both",
				access = "primary",
				admin = true,
				params = object({ line = string, input = command_input, session_id = string }, { "line" }),
				order = { "line", "input", "..." },
				result = object({ result = {} }),
				errors = { "unknown_command", "command_unavailable", "command_failed", "local_only" },
			},
			register = {
				doc = "Register a slash command or alias (`alias_of`) owned by the caller; remote callers must pass `replace = true` to overwrite an existing name, and releasing a registration restores any previous definition.",
				expose = "both",
				access = "primary",
				admin = true,
				registration = true,
				params = object({
					name = string,
					description = string,
					args = string,
					auto_execute = boolean,
					local_only = boolean,
					availability = availability,
					alias_of = string,
					replace = boolean,
				}, { "name" }),
				order = { "name", "..." },
				callbacks = {
					run = {
						type = "fun(args: string, input?: mare.CommandInput): any",
						params = object({ args = string, input = command_input }, { "args" }),
						order = { "args", "input" },
						result = {},
						optional = true,
					},
					complete = {
						type = "fun(prefix: string): table[]",
						params = object({ prefix = string }, { "prefix" }),
						order = { "prefix" },
						result = { type = "array", items = completion_item },
						optional = true,
					},
				},
			},
		},
	},
}
