---@meta _
-- The `status` service contract. Part of the built-in contracts (see contracts/schema.lua).
local S = ...
local object, integer, string = S.object, S.integer, S.string
local status_snapshot = object({
	state = { type = "string", enum = { "idle", "working", "blocked" } },
	message = string,
	session = object({
		session_id = string,
		path = string,
		resume = { type = "array", items = string },
	}),
	cwd = string,
	model = string,
	pid = integer,
	seq = integer,
}, { "state", "message", "session", "cwd", "pid", "seq" })
-- Lua declares it as mare.Status (shared with status/changed).
status_snapshot.title = "mare.Status"
return {
	["status"] = {
		doc = "Unified agent status of the process. Replaceable as a whole.",
		methods = {
			get = {
				doc = "The current unified agent status.",
				expose = "both",
				access = "peer_default",
				params = object({}),
				order = {},
				result = status_snapshot,
			},
		},
		events = {
			["exit"] = {
				doc = "Process exit status snapshot published at clean shutdown.",
				mode = "emit",
				expose = "lua",
				data = status_snapshot,
			},
			["session"] = {
				doc = "Active session metadata inside status updated.",
				mode = "emit",
				expose = "lua",
				data = status_snapshot,
			},
		},
	},
}
