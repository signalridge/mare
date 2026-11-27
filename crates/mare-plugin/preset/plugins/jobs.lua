-- jobs: background tasks.
-- Processes and their output buffers live in Rust (mare.proc.start: bounded ring, absolute
-- offsets); this plugin owns ids, ownership, the job_* tools, completion notifications,
-- session records and the UI. Anything with the handle interface below can be tracked, so
-- sub-agents or terminals can register as jobs too:
--   handle:status() -> {state="running"|"exited", code, signal, elapsed_ms, unread}
--   handle:read{max_bytes} -> {text, dropped, done} (consuming cursor)
--   handle:tail(n) -> string (non-consuming, live view)
--   handle:wait{timeout_ms, cancel} -> status handle:kill(signal)
--
-- Events (emit): jobs/changed {job}

return {
	name = "jobs",
	inject = { "tools", "bus", "task" },
	provide = { "jobs" },
	config_schema = {
		buffer_bytes = { type = "integer", default = 1024 * 1024, desc = "Ring buffer per job." },
		max_running = { type = "integer", default = 8, desc = "Concurrent jobs per agent." },
		max_jobs = {
			type = "integer",
			default = 64,
			desc = "Jobs kept in total; finished ones beyond this are pruned, least recently used first.",
		},
		wait_timeout_ms = { type = "integer", default = 30000, desc = "Default wait of job_output." },
		max_wait_ms = { type = "integer", default = 600000, desc = "Cap of the job_output wait." },
		read_max_bytes = { type = "integer", default = 50 * 1024, desc = "Most job_output returns per call." },
		notify_inline_bytes = {
			type = "integer",
			default = 2048,
			desc = "Completion notices include output up to this size.",
		},
		max_wakeups = {
			type = "number",
			desc = "Consecutive wake-ups of an idle agent without user input (nil = no limit).",
		},
		batch_ms = {
			type = "integer",
			default = 50,
			desc = "Jobs finishing within this window share one notification.",
		},
		kill_grace_ms = {
			type = "integer",
			default = 3000,
			desc = "SIGTERM, then SIGKILL after this long.",
		},
		widget = { type = "boolean", default = true, desc = "Running-jobs line above the input box." },
		view_tail_bytes = {
			type = "integer",
			default = 64 * 1024,
			desc = "/jobs output view: the last this many bytes.",
		},
		view_refresh_ms = { type = "integer", default = 500, desc = "/jobs output view refresh while open." },
	},
	apply = function(ctx, cfg)
		-- structural submodules (plugins/lua/jobs/), loaded here in dependency order
		local D = { ctx = ctx, cfg = cfg }
		-- the optional services, read on each call
		D.svc = require("mare_lib").followers(ctx, { "agent", "session" })
		D.format = require("jobs.format")
		D.notify = require("jobs.notify")(D)
		D.registry = require("jobs.registry")(D)
		require("jobs.tools")(D)
		require("jobs.view")(D)
	end,
}
