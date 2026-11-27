---@meta _
-- The `terminal` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, integer, string, boolean, map, list_of = S.object, S.integer, S.string, S.boolean, S.map, S.list_of

local merged = S.merged

local function list_merged(base, extra)
	local out = { table.unpack(base) }
	for _, v in ipairs(extra) do
		out[#out + 1] = v
	end
	return out
end

local terminal_signal = { type = "string", enum = { "SIGINT", "SIGTERM", "SIGKILL", "SIGTSTP", "SIGHUP" } }
local terminal_status = { type = "string", enum = { "running", "completed", "failed", "cancelled" } }
local snapshot_props = {
	terminal_id = string,
	name = string,
	pid = integer,
	cols = integer,
	rows = integer,
	status = terminal_status,
	exit_code = integer,
	exit_signal = integer,
	command_exit_code = integer,
	output_offset = integer,
	truncated = boolean,
	screen = string,
	view_truncated = boolean,
	alternate_screen = boolean,
	prompt_seq = integer,
}
local snapshot_req = { "terminal_id", "cols", "rows", "status", "output_offset", "truncated" }
local terminal_snapshot = object(snapshot_props, snapshot_req)
terminal_snapshot.title = "mare.TerminalSessionSnapshot"

local max_bytes_send_schema = {
	type = "integer",
	minimum = 1,
	maximum = 4194304,
	description = "Maximum output bytes to return (1..4194304, default 262144). Output is truncated at a UTF-8 boundary (always returning at least one character if non-empty) and sets `truncated` true; when `screen` is true, rendered screen is head-trimmed to fit and sets `view_truncated`.",
}
local max_bytes_read_schema = {
	type = "integer",
	minimum = 1,
	maximum = 4194304,
	description = "Maximum output bytes to return (1..4194304, default 262144). Applies to both incremental `output_offset` slicing (UTF-8 safe, floor one char) and VT page mode as well as `screen`; sets `truncated` true when capped.",
}
local terminal_id_schema = {
	type = "string",
	minLength = 1,
	description = "Owned terminal identifier returned by `terminal.create`.",
}
local cols_schema =
	{ type = "integer", minimum = 1, maximum = 500, description = "Terminal columns (1..500, default 80)." }
local rows_schema =
	{ type = "integer", minimum = 1, maximum = 200, description = "Terminal rows (1..200, default 24)." }
local wait_ms_schema = {
	type = "integer",
	minimum = 0,
	maximum = 60000,
	description = "Optional bounded wait in milliseconds (0..60000).",
}

return {
	["terminal"] = {
		doc = "Owner-bound interactive PTY sessions (closed automatically when the owning plugin or RPC connection disconnects). Returned text is capped by `max_bytes` (default 262144 bytes).",
		errors = {
			unknown_terminal = {
				doc = "no owned terminal with that terminal_id (including terminals created by another connection)",
			},
			terminal_limit_reached = { doc = "maximum active terminals (16) per owner reached" },
			duplicate_name = { doc = "another active terminal for this owner already uses that name" },
			not_running = { doc = "the terminal has already finished and cannot accept input, signals, or resize" },
			stop_unconfirmed = {
				doc = "the terminal process group did not confirm termination within the grace period",
			},
		},
		methods = {
			create = {
				doc = "Create an owner-bound PTY session; automatically closed when the caller context or RPC connection ends.",
				expose = "both",
				access = "primary",
				admin = true,
				scope = "session",
				params = object({
					name = {
						type = "string",
						minLength = 1,
						maxLength = 128,
						description = "Optional unique name within this owner (1..128 bytes).",
					},
					cmd = string,
					args = { type = "array", items = string },
					cwd = string,
					env = map(string),
					cols = cols_schema,
					rows = rows_schema,
					buffer_bytes = { type = "integer", minimum = 1024, maximum = 4194304 },
					scrollback_lines = { type = "integer", minimum = 0 },
					emulate = boolean,
				}),
				order = { "..." },
				result = terminal_snapshot,
				errors = { "terminal_limit_reached", "duplicate_name" },
			},
			send = {
				doc = "Send input bytes/text to an owned PTY and optionally wait for output/exit, capped at `max_bytes` bytes.",
				expose = "both",
				access = "primary",
				scope = "session",
				params = object({
					terminal_id = terminal_id_schema,
					text = string,
					submit = boolean,
					wait_ms = wait_ms_schema,
					max_bytes = max_bytes_send_schema,
					screen = boolean,
				}, { "terminal_id" }),
				order = { "..." },
				result = object(
					merged(snapshot_props, { output = string, next_output_offset = integer, timed_out = boolean }),
					list_merged(snapshot_req, { "output", "next_output_offset", "timed_out" })
				),
				errors = { "unknown_terminal", "not_running" },
			},
			read = {
				doc = "Read incremental `output_offset` or VT-paged output from an owned PTY, capped at `max_bytes` bytes.",
				expose = "both",
				access = "peer",
				scope = "session",
				params = object({
					terminal_id = terminal_id_schema,
					offset = { type = "integer", minimum = 0 },
					count = { type = "integer", minimum = 1, maximum = 100000 },
					output_offset = { type = "integer", minimum = 0 },
					max_bytes = max_bytes_read_schema,
					screen = boolean,
					wait_ms = wait_ms_schema,
				}, { "terminal_id" }),
				order = { "..." },
				result = object(
					merged(snapshot_props, {
						text = string,
						total_lines = integer,
						line_begin = integer,
						line_end = integer,
						next_output_offset = integer,
					}),
					list_merged(snapshot_req, { "text", "total_lines", "line_begin", "line_end", "next_output_offset" })
				),
				errors = { "unknown_terminal" },
			},
			signal = {
				doc = "Deliver a signal to the foreground process group of an owned PTY.",
				expose = "both",
				access = "primary",
				scope = "session",
				params = object(
					{ terminal_id = terminal_id_schema, signal = terminal_signal },
					{ "terminal_id", "signal" }
				),
				order = { "..." },
				result = object({}),
				errors = { "unknown_terminal", "not_running" },
			},
			resize = {
				doc = "Resize an owned PTY window.",
				expose = "both",
				access = "primary",
				scope = "session",
				params = object(
					{ terminal_id = terminal_id_schema, cols = cols_schema, rows = rows_schema },
					{ "terminal_id", "cols", "rows" }
				),
				order = { "..." },
				result = object({}),
				errors = { "unknown_terminal", "not_running" },
			},
			close = {
				doc = "Stop and close an owned PTY session cleanly.",
				expose = "both",
				access = "primary",
				scope = "session",
				params = object({
					terminal_id = terminal_id_schema,
					grace_ms = {
						type = "integer",
						minimum = 0,
						maximum = 30000,
						description = "Optional SIGTERM grace period in milliseconds (0..30000, default 3000).",
					},
				}, { "terminal_id" }),
				order = { "..." },
				result = terminal_snapshot,
				errors = { "unknown_terminal", "stop_unconfirmed" },
			},
			list = {
				doc = "List PTY sessions owned by the caller connection or plugin.",
				expose = "both",
				access = "peer",
				scope = "session",
				params = object({}),
				order = {},
				result = list_of(terminal_snapshot),
			},
		},
	},
}
