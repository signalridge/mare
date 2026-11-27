---@meta _
-- The `proc` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, integer, string, boolean = S.object, S.integer, S.string, S.boolean
local size = object({ rows = integer, cols = integer }, { "rows", "cols" })
local function map(values)
	return { type = "object", additionalProperties = values }
end
return {
	["proc"] = {
		doc = "Caller-owned sandboxed piped and PTY child processes (Lua-only primitive).",
		errors = {
			process_limit_reached = { doc = "too many active processes for this owner; kill or wait for one to exit" },
			unknown_process = { doc = "no active process with that process_id for this owner" },
		},
		methods = {
			spawn = {
				doc = "Start a sandboxed piped or PTY child process bound to the caller lifecycle.",
				expose = "lua",
				admin = true,
				callbacks = {
					on_output = 'fun(ev: {process_id: string, stream: "stdout"|"stderr"|"pty", data_base64: string})',
					on_exit = "fun(ev: {process_id: string, exit_code?: integer, output_drain_timed_out?: boolean})",
				},
				params = object({
					process_id = string,
					command = { type = "array", items = string },
					cwd = string,
					env = map(string),
					tty = boolean,
					size = size,
				}, { "process_id", "command" }),
				order = { "..." },
				result = object({ process_id = string }, { "process_id" }),
				errors = { "process_limit_reached" },
			},
			write = {
				doc = "Write base64 bytes to stdin or PTY of an owned process, optionally closing piped stdin.",
				expose = "lua",
				params = object({
					process_id = string,
					data_base64 = string,
					close_stdin = boolean,
				}, { "process_id" }),
				order = { "..." },
				result = object({}),
				errors = { "unknown_process" },
			},
			resize = {
				doc = "Resize the PTY of an owned process started with tty = true.",
				expose = "lua",
				params = object({
					process_id = string,
					size = size,
				}, { "process_id", "size" }),
				order = { "..." },
				result = object({}),
				errors = { "unknown_process" },
			},
			kill = {
				doc = "Terminate an owned running process and its process group.",
				expose = "lua",
				params = object({
					process_id = string,
				}, { "process_id" }),
				order = { "..." },
				result = object({}),
				errors = { "unknown_process" },
			},
		},
	},
}
