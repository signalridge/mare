-- tools: registry (validation, execution, timeout, result truncation) + read/write/edit/bash.
-- Edit tool per model: edit_mode = "auto" | "apply_patch" | "replace".
--
-- Tool definition:
--   { description=, parameters=<JSON schema>, execute=function(args, tctx) -> result,
--     (tctx = { cancel, update, cwd, tool_call_id, idempotency_key })
--     summary=function(args) -> "Read(src/main.rs)", render=function(args, result, expanded) -> node,
--     enabled=function(model, mode) -> bool }
-- result = { content=string|blocks, is_error=bool, details=table, display=node }

local MAX_LINES = 2000
local MAX_BYTES = 50 * 1024

return {
	name = "tools",
	-- in the minimal plugin set
	tags = { "minimal" },
	-- config functions this plugin offers to init.lua (installed by the core before init.lua runs)
	config_api = {
		--- mare.tool.command { name, description, command = {..., "{param}"}, params }
		["tool.command"] = function(C, def)
			C.tool_commands = C.tool_commands or {}
			table.insert(C.tool_commands, def)
		end,
		--- mare.tools.disable { "ls", ... }
		["tools.disable"] = function(C, names)
			C.tools_disabled = C.tools_disabled or {}
			for _, n in ipairs(names) do
				table.insert(C.tools_disabled, n)
			end
		end,
	},
	provide = { "tools" },
	inject = { "bus", "policy", "task", "kernel" },
	config_schema = {
		search_results = { type = "integer", default = 5, desc = "Default result count of tool_search." },
		edit_mode = {
			type = "enum",
			choices = { "auto", "apply_patch", "replace" },
			default = "auto",
			desc = 'Edit tool: "auto" (apply_patch for GPT models, replace otherwise), "apply_patch" or "replace".',
		},
		-- read before edit: edit / apply_patch /
		-- overwriting write need a read of the file in this session, and refuse if it changed since
		read_before_edit = {
			type = "boolean",
			default = true,
			desc = "Require a read of a file in this session before editing or overwriting it.",
		},
		timeout_ms = { type = "integer", default = 600000, desc = "Generic per-tool timeout." },
		bash_timeout_ms = { type = "integer", default = 120000, desc = "Default bash timeout." },
		bash_max_timeout_ms = { type = "integer", default = 600000, desc = "Largest bash timeout a call may ask for." },
		bash_read_bytes = {
			type = "integer",
			default = 4 * 1024 * 1024,
			desc = "Most output a finished command returns from its job buffer.",
		},
		max_lines = { type = "integer", default = MAX_LINES, desc = "Tool output truncation: lines." },
		max_bytes = { type = "integer", default = MAX_BYTES, desc = "Tool output truncation: bytes." },
		max_line_chars = {
			type = "integer",
			default = 500,
			desc = "Search output: per-line cap.",
		},
		read_max_line_bytes = {
			type = "integer",
			default = 2000,
			desc = "Read: lines longer than this are cut on a UTF-8 boundary.",
		},
		image_max_dim = { type = "integer", default = 2000, desc = "Images read by `read` are downscaled to fit." },
		-- files at or above this size are read by streaming the requested window; edit /
		-- apply_patch refuse larger files
		read_stream_min_bytes = {
			type = "integer",
			default = 10 * 1024 * 1024,
			desc = "Files at or above this size are read by streaming the requested window.",
		},
		edit_max_bytes = {
			type = "integer",
			default = 10 * 1024 * 1024,
			desc = "Largest file edit / apply_patch accept.",
		},
		-- the persistent shell keeps at most this much of one command's output in memory; the
		-- rest goes to a spill file
		shell_buffer_bytes = {
			type = "integer",
			default = 1024 * 1024,
			desc = "Output of one command the persistent shell keeps in memory.",
		},
		ls_limit = { type = "integer", default = 500, desc = "Default `ls` entry limit." },
		live_tail_bytes = { type = "integer", default = 4096, desc = "Running-command output shown in the UI." },
		update_interval_ms = { type = "integer", default = 150, desc = "UI update throttle for running commands." },
		cancel_grace_ms = {
			type = "integer",
			default = 2000,
			desc = "After cancellation, how long a tool may take to stop by itself.",
		},
		shell = {
			type = "string",
			desc = "Path of the shell; default $SHELL (bash/zsh) or bash; Windows: Git Bash.",
		},
		-- Windows: also offer a `powershell` tool (pwsh, else Windows PowerShell). "replace" swaps it
		-- in for the model-facing bash tool (`!cmd` keeps using bash)
		powershell = {
			type = { "string", "boolean" },
			default = "auto",
			desc = 'PowerShell tool: "auto" (on Windows), true, false or "replace".',
		},
		prelude = {
			type = { "string", "table" },
			desc = "String or list run before every shell command (bash tool, `!cmd`), e.g. 'eval \"$(mise env -s bash)\"'.",
		},
		-- environment for spawned commands: the parent
		-- environment minus /KEY|PASSWORD|SECRET|TOKEN/i and MARE_* names. include: names to keep
		-- anyway (e.g. "GH_TOKEN"); exclude: more names to drop; set: name -> value added after
		env = {
			type = "object",
			desc = "Environment of spawned commands.",
			fields = {
				include = {
					type = "list",
					items = { type = "string" },
					default = {},
					desc = "Names to keep despite the scrubbing.",
				},
				exclude = { type = "list", items = { type = "string" }, default = {}, desc = "More names to drop." },
				set = { type = "map", default = {}, desc = "Name to value, added after scrubbing." },
			},
		},
		-- bash runs in one persistent shell per agent (
		-- shell/tool-bash-persistent): cwd, exported variables and functions carry over; "fresh":
		-- a new shell for every call, starting in the session directory
		shell_session = {
			type = "string",
			default = "persistent",
			desc = '"persistent" (one shell per agent) or "fresh" (a new shell per call).',
		},
		-- a command silent this long is returned as "still running" (moved to a background job,
		-- once per silence); 0 = only the timeout
		stall_ms = {
			type = "integer",
			default = 30000,
			desc = 'A command silent this long is returned as "still running"; 0 = only the timeout.',
		},
	},
	apply = function(ctx, cfg)
		MAX_LINES, MAX_BYTES = cfg.max_lines, cfg.max_bytes
		-- structural submodules (plugins/lua/tools/): tool metadata registers eagerly so
		-- `tools:list()` and `permissions` shadowing see every tool immediately, while execution
		-- pipeline, file guards, shell environment, and persistent bash load on first use via
		-- `mare_lib.lazy` / `mare_lib.lazy_group`.
		local lib = require("mare_lib")
		local D = {
			ctx = ctx,
			cfg = cfg,
			policy = ctx.policy --[[@as mare.PolicyService]],
			lazy_bash_run = true,
			initial_shell_cwd = nil,
		}
		-- services this plugin reads when they are up: D.svc.<name> or nil
		D.svc = lib.followers(ctx, { "llm", "session", "permissions", "approvals", "agent", "jobs" })
		D.registry = require("tools.registry")(D)
		local tools = D.registry.tools

		local ensure_pipeline = lib.lazy_group(tools, { "execute", "output_channel", "send_output" }, function()
			D.approval = require("tools.approval")(D)
			D.results = require("tools.results")(D)
			D.output_channel = require("tools.output_channel")(D)
			D.pipeline = require("tools.pipeline")(D)
			D.nested = require("tools.nested")(D)
		end)
		for _, field in ipairs({ "approval", "results", "output_channel", "pipeline", "nested" }) do
			D[field] = setmetatable({}, {
				__index = function(_, k)
					ensure_pipeline()
					return D[field][k]
				end,
			})
		end
		require("tools.executors")(D)
		D.search = require("tools.search")(D)
		local ensure_call = lib.lazy(function()
			return require("tools.call")(D)
		end)
		-- Some models stringify `arguments` as a JSON string despite the object schema.
		-- Decode it before outer schema validation so the wire schema stays a clean object.
		D.policy:rewriter("tool/rewrite", function(call)
			if call.name ~= "call_tool" or type(call.arguments) ~= "table" then
				return nil
			end
			if type(call.arguments.arguments) ~= "string" then
				return nil
			end
			return ensure_call().rewrite(call)
		end, { prepend = true })
		D.dispatchers = { call_tool = true }
		tools.register(tools, ctx, "call_tool", {
			snippet = "Invoke a deferred tool by name and arguments",
			description = "Invoke a deferred tool discovered via tool_search. Pass the exact tool name "
				.. "and a JSON object of arguments matching its input schema.",
			parameters = {
				type = "object",
				properties = {
					name = { type = "string", description = "Exact name of the deferred tool to invoke" },
					arguments = {
						type = "object",
						description = "Arguments object matching the target tool's input schema",
						additionalProperties = true,
					},
				},
				required = { "name" },
			},
			-- Outer call_tool is a pure dispatcher; the inner deferred tool carries its own
			-- side_effect, unconfined flag, permissions vote, timeout budget, and approval prompt.
			side_effect = "read",
			unconfined = false,
			allow_nested = false,
			timeout_policy = "managed",
			enabled = function(model)
				if D.registry.native_tool_update(model) then
					return false
				end
				return D.search ~= nil and D.search.has_deferred()
			end,
			concurrency_safe = function(args)
				return ensure_call().concurrency_safe(args)
			end,
			summary = function(args)
				return ensure_call().summary(args)
			end,
			execute = function(args, tctx)
				return ensure_call().execute(args, tctx)
			end,
		})
		D.file_guard = select(
			2,
			lib.lazy_group(tools, { "observe", "observation", "with_file_lock", "fs_guard", "fs_commit" }, function()
				return require("tools.file_guard")(D)
			end, {
				fast_unloaded = {
					observation = function()
						return nil
					end,
				},
			})
		)
		D.file_tools = require("tools.file_tools")(D)
		D.shell_env = select(
			2,
			lib.lazy_group(tools, { "shell_command", "process_env", "env_policy" }, function()
				return require("tools.shell_env")(D)
			end)
		)
		local ensure_bash_output = lib.lazy(function()
			return require("tools.bash_output")(D)
		end)
		D.bash_output = setmetatable({}, {
			__index = function(_, k)
				return ensure_bash_output()[k]
			end,
		})
		-- Remove dead-PID bash spill files in the background without eagerly loading `tools.bash_output`.
		D.spill_cleaned = true
		ctx:spawn(function()
			local own_start = mare.proc.start_time and mare.proc.start_time()
			local dir = mare.paths.state .. "/spill"
			local ok, entries = pcall(mare.fs.list, dir)
			if not ok or type(entries) ~= "table" then
				return
			end
			for _, e in ipairs(entries) do
				local name = type(e) == "table" and e.name or e
				if type(name) == "string" and name:sub(-4) == ".log" then
					local pid_s, start_s = string.match(name, "^bash%-(%d+)%-s(%d+)%-")
					local pid = tonumber(pid_s) or tonumber(string.match(name, "^bash%-(%d+)%-"))
					local start = tonumber(start_s)
					local is_self = pid == mare.pid and (not start or not own_start or start == own_start)
					if pid and not is_self and not mare.proc.alive(pid, start) then
						pcall(mare.fs.remove, dir .. "/" .. name)
					end
				end
			end
		end)
		local ensure_bash_run, bash_run_proxy = lib.lazy_group(tools, { "cwd" }, function()
			return require("tools.bash_run")(D)
		end, {
			fast_unloaded = {
				cwd = function()
					return D.initial_shell_cwd or mare.cwd
				end,
			},
		})
		D.bash_run = bash_run_proxy
		function D.detach_background()
			return false
		end
		function D.on_session_loaded(ev)
			local base = (D.svc.session and D.svc.session.info and D.svc.session:info().cwd) or mare.cwd
			D.initial_shell_cwd = (ev and type(ev.shell_cwd) == "string") and ev.shell_cwd
				or (base ~= mare.cwd and base or nil)
		end
		function D.on_session_new(ev)
			local base = (ev and type(ev.cwd) == "string" and ev.cwd ~= "") and ev.cwd
				or (D.svc.session and D.svc.session.info and D.svc.session:info().cwd)
				or mare.cwd
			D.initial_shell_cwd = base ~= mare.cwd and base or nil
		end
		function D.on_compact_completed(ev)
			if D.initial_shell_cwd and D.initial_shell_cwd ~= mare.cwd then
				ensure_bash_run()
				D.on_compact_completed(ev)
			end
		end

		ctx:inject({ "ui" }, function(ui_ctx)
			ui_ctx.ui:action("background", function()
				return D.detach_background()
			end, {
				desc = "move the running command to the background",
				contexts = { "editor" },
				keys = { editor = "<C-b>" },
			})
		end)
		ctx:on("session/loaded", function(ev)
			D.on_session_loaded(ev)
		end)
		ctx:on("session/new", function(ev)
			D.on_session_new(ev)
		end)
		ctx:on("compact/completed", function(ev)
			D.on_compact_completed(ev)
		end)

		require("tools.shell_tools")(D)
		D.file_tools.register_ls()
		require("tools.command_tools")(D)
		-- a tool's `enabled` may read a query backend (session-search: query:supports): one coming
		-- or going changes what the next request declares, which the agent caches per generation
		ctx:on("query/changed", function()
			tools.invalidate(tools, ctx)
		end)

		-- a mare.Service: methods receive the traced caller ctx
		lib.service(ctx, "tools", tools)
		require("tools.prompt")(D)
	end,
}
