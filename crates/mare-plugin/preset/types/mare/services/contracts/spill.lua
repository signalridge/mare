---@meta _
-- The `spill` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, integer, string, boolean, list_of = S.object, S.integer, S.string, S.boolean, S.list_of
local backend_info = object({
	name = string,
	selected = boolean,
}, { "name", "selected" })
local save_ref = object({
	spill_id = string,
	size = integer,
}, { "spill_id", "size" })
local read_result = object({
	content = string,
	data_base64 = string,
	next_offset = integer,
	size = integer,
})
return {
	["spill"] = {
		doc = "Session-owned recovery spill storage and storage backends.",
		events = {
			["backends-changed"] = {
				doc = "Registered spill storage backends changed.",
				mode = "emit",
				expose = "lua",
				retain = "last",
				data = object({ name = string, kind = string }, { "name", "kind" }),
			},
		},
		errors = {
			unknown_backend = { doc = "no spill backend is registered under that name" },
			unknown_spill = {
				doc = "no spill object with that spill_id exists, or it was released or owned by another session",
			},
		},
		methods = {
			list_backends = {
				doc = "Registered spill storage backends, with which one is currently selected.",
				expose = "both",
				access = "peer",
				scope = "host",
				params = object({}),
				order = {},
				result = list_of(backend_info),
			},
			save = {
				doc = "Persist UTF-8 text (`content`) or strict padded base64 bytes (`data_base64`, max 8 MiB decoded) in the active session's spill storage.",
				expose = "both",
				access = "primary",
				scope = "session",
				params = object({
					content = string,
					data_base64 = string,
					backend = string,
				}),
				order = { "..." },
				result = save_ref,
				errors = { "unknown_backend" },
			},
			read = {
				doc = 'Read a UTF-8 text page (default) or base64 byte page (`encoding = "base64"`) from a session-owned spill object.',
				expose = "both",
				access = "peer",
				scope = "session",
				params = object({
					spill_id = string,
					encoding = { type = "string", enum = { "text", "base64" } },
					offset = integer,
					limit = integer,
				}, { "spill_id" }),
				order = { "spill_id", "..." },
				result = read_result,
				errors = { "unknown_backend", "unknown_spill" },
			},
			delete = {
				doc = "Invalidate and delete a session-owned spill object.",
				expose = "both",
				access = "primary",
				scope = "session",
				params = object({ spill_id = string }, { "spill_id" }),
				order = { "spill_id" },
				result = object({}),
				errors = { "unknown_backend", "unknown_spill" },
			},
			register = {
				doc = "Register a named spill backend; removed automatically on disposal.",
				expose = "lua",
				registration = true,
				lua = {
					name = "string",
					backend = "mare.LocalSpill",
				},
				order = { "name", "backend" },
			},
			save_owned = {
				doc = "Save UTF-8 text owned by an explicit `session_id` (Lua-only; includes host `path`).",
				expose = "lua",
				lua_unchecked = true,
				lua = {
					session_id = "string",
					content = "string",
					opts = "(mare.SpillSaveOpts|{backend?: string})?",
				},
				order = { "session_id", "content", "opts" },
				result = { type = "object", additionalProperties = true },
				lua_result = "mare.SpillServiceRef",
			},
			save_bytes = {
				doc = "Save raw binary bytes (max 8 MiB) owned by an explicit `session_id` (Lua-only; includes host `path`).",
				expose = "lua",
				lua_unchecked = true,
				lua = {
					session_id = "string",
					bytes = "string",
					opts = "(mare.SpillSaveOpts|{backend?: string})?",
				},
				order = { "session_id", "bytes", "opts" },
				result = { type = "object", additionalProperties = true },
				lua_result = "mare.SpillServiceRef",
			},
			read_owned = {
				doc = "Read UTF-8 text owned by an explicit `session_id` (Lua-only).",
				expose = "lua",
				lua_unchecked = true,
				lua = {
					session_id = "string",
					spill_id = "string",
					opts = "{offset?: integer, limit?: integer}?",
				},
				order = { "session_id", "spill_id", "opts" },
				result = object({ content = string, next_offset = integer }, { "content" }, true),
			},
			read_bytes = {
				doc = "Read raw binary bytes owned by an explicit `session_id` (Lua-only; used by UI image materializers).",
				expose = "lua",
				lua_unchecked = true,
				lua = {
					session_id = "string",
					spill_id = "string",
					opts = "{offset?: integer, limit?: integer}?",
				},
				order = { "session_id", "spill_id", "opts" },
				result = object({ bytes = string, next_offset = integer, size = integer }, { "bytes", "size" }, true),
			},
			delete_owned = {
				doc = "Delete a spill object owned by an explicit `session_id` (Lua-only).",
				expose = "lua",
				lua_unchecked = true,
				lua = {
					session_id = "string",
					spill_id = "string",
				},
				order = { "session_id", "spill_id" },
				result = boolean,
			},
		},
	},
}
