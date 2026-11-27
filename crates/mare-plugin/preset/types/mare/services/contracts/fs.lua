---@meta _
-- The `fs` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, string, integer, boolean, list_of = S.object, S.string, S.integer, S.boolean, S.list_of
local open_object = { type = "object", additionalProperties = true }
local list_entry = object({
	file_name = string,
	is_file = boolean,
	is_directory = boolean,
}, { "file_name", "is_file", "is_directory" })
local stat_result = object({
	is_file = boolean,
	is_directory = boolean,
	size = integer,
	created_at_ms = integer,
	modified_at_ms = integer,
	mode = integer,
	nlink = integer,
}, { "is_file", "is_directory", "size", "created_at_ms", "modified_at_ms" })
return {
	["fs"] = {
		doc = "Policy-constrained filesystem read, write, directory list and stat.",
		events = {
			["watch-event"] = {
				doc = "Internal watch delta emitted to the owning RPC connection (forwarded as `fs/changed`).",
				mode = "emit",
				expose = "process",
				data = object({
					owner = integer,
					watch_id = string,
					changed_paths = { type = "array", items = string },
					truncated = boolean,
				}, { "owner", "watch_id", "changed_paths" }, true),
			},
			["changes-inexact"] = {
				doc = "A file edit tool could not establish exact diff line bounds.",
				mode = "emit",
				expose = "lua",
				emitters = { "tools", "tools-apply-patch" },
				data = object({ tool = string, paths = { type = "array", items = string } }, { "tool", "paths" }),
			},
			["edit-intent"] = {
				doc = "Policy gate evaluated before editing an existing workspace file.",
				mode = "policy",
				expose = "lua",
				emitters = { "tools", "tools-apply-patch" },
			},
			["observed"] = {
				doc = "A file path was observed (read or stat recorded).",
				mode = "emit",
				expose = "lua",
				emitters = { "tools", "tools-apply-patch" },
				data = object({ path = string, observation = { type = "object", additionalProperties = true } }, {
					"path",
					"observation",
				}),
			},
			["write-intent"] = {
				doc = "Policy gate evaluated before writing a workspace file.",
				mode = "policy",
				expose = "lua",
				emitters = { "tools", "tools-apply-patch" },
			},
		},
		errors = {
			not_found = { doc = "the target file or directory does not exist" },
			already_exists = { doc = "the target path already exists when create_new is set" },
			too_large = { doc = "the file size exceeds the requested max_bytes limit" },
			io_failed = {
				doc = "the filesystem operation could not be completed due to an I/O or decode error",
			},
		},
		methods = {
			read = {
				doc = "Read a file as base64, bounded by active sandbox policy and optional max_bytes.",
				expose = "both",
				confined = true,
				access = "peer",
				params = object({
					path = string,
					max_bytes = integer,
				}, { "path" }),
				order = { "path", "..." },
				result = object({ data_base64 = string }, { "data_base64" }),
				errors = { "not_found", "too_large", "io_failed" },
			},
			write = {
				doc = "Write base64 bytes to a file within writable sandbox roots and protected-path exclusions.",
				expose = "both",
				confined = true,
				access = "primary",
				admin = true,
				params = object({
					path = string,
					data_base64 = string,
					atomic = boolean,
					create_new = boolean,
				}, { "path", "data_base64" }),
				order = { "path", "data_base64", "..." },
				result = object({}),
				errors = { "already_exists", "not_found", "io_failed" },
			},
			list = {
				doc = "List directory entries filtered by the active sandbox read policy.",
				expose = "both",
				confined = true,
				access = "peer",
				params = object({ path = string }, { "path" }),
				order = { "path" },
				result = list_of(list_entry),
				errors = { "not_found", "io_failed" },
			},
			stat = {
				doc = "Inspect file metadata within the active sandbox read policy.",
				expose = "both",
				confined = true,
				access = "peer",
				params = object({ path = string }, { "path" }),
				order = { "path" },
				result = stat_result,
				errors = { "not_found", "io_failed" },
			},
			watch = {
				doc = "Watch a workspace path for policy-filtered filesystem changes (emits `fs/changed`; release via `registrations.release`).",
				expose = "both",
				confined = true,
				access = "primary",
				registration = true,
				host_local = true,
				params = object({ path = string }, { "path" }),
				order = { "path" },
			},
		},
	},
}
