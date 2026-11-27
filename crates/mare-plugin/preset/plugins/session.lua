-- session: session tree persisted as JSONL (every entry has entry_id + parent_entry_id). Resume, fork, clone,
-- tree navigation. Entries:
--   {type="session", version=mare.store.format_version, session_id, cwd, created_at_ms, parent_session_id?} header (first line)
--   {type="message", entry_id, parent_entry_id, created_at_ms, message}
--   {type="compaction", entry_id, parent_entry_id, created_at_ms, summary, first_kept_entry_id, tokens_before, tokens_after?, method?,
--    range?={from_entry_id, to_entry_id}, preserved?={user messages kept verbatim}, remote?={provider, model, items}}
--      method = "local" (windowed summary) | "remote"
--   {type="name", name} {type="leaf", target_entry_id} {type="model", model, effort, at_entry_id}
--   {type="branch_summary", entry_id, parent_entry_id, from_entry_id, created_at_ms, summary, files} abandoned-branch summary
--   {type="custom", kind="agent/queue", data={steer, follow_up}, created_at_ms} queued inputs
--   {type="prune", target_entry_ids={[entry_id]=content}, method?, tokens_before?, tokens_after?, created_at_ms}
--      method = "prune" (old tool outputs → head + tail tool-result pruner) | "offload"
--      (images → attachment files image-offload); originals stay in the log
--   {type="custom", kind, data, entry_id?, at_entry_id?, created_at_ms} plugin records (session:record)
--   {type="state", state="running"|"closed", created_at_ms} written when a writer opens / cleanly closes the
--      file: a resume that finds no "closed" after the last "running" follows a crash (
--      part four counts only real crashes, never a clean shutdown — no pid check, pids recycle)

local PLUGIN_NAME = "session"

return {
	name = PLUGIN_NAME,
	-- in the minimal plugin set
	tags = { "minimal" },
	-- `task` gives ctx:spawn, `bus` the mare events, `policy` the session decision points, `kernel`
	-- the reload generation a carried session is stamped with (startup.lua); llm and compact are
	-- followed (D.follow), commands below
	inject = { "agent", "task", "bus", "policy", "kernel" },
	provide = { "session" },
	config_schema = {
		branch_summary = {
			type = "boolean",
			default = true,
			desc = "Summarize the branch left behind when switching branches.",
		},
		transcript_poll_ms = {
			type = "integer",
			default = 500,
			desc = "Live refresh of an open sub-session transcript.",
		},
		-- resuming a session recorded in another directory:
		-- "ask" (TUI; headless uses the current one), "current", "session". "session" moves the
		-- shell there (bash commands run in it); file tools keep the launch directory
		resume_cwd = {
			type = "enum",
			choices = { "ask", "current", "session" },
			default = "ask",
			desc = "Directory used when resuming a session recorded in another one.",
		},
		-- /resume rows (the look is data, session/picker_rows.lua): "comfortable" (the title,
		-- then a dim row of time · short id · directory · branch), "dense" (one row's dense
		-- view) or "cards"; Ctrl+L in
		-- the picker steps through them. A look is rows of segment names — `picker_looks = { mine =
		-- { { "title", "mark" }, { "time", "dir", sep = " · " } } }` — from title, time, time_col,
		-- mark, id, dir, branch, messages, labels, input, or your own: `picker_segments = { name =
		-- function(row, info) return text, highlight end }` (row: the session index row; info:
		-- {current, open = peer name?, crashed, short = short id}). `picker_split`: the list's
		-- share of a wide picker next to its preview, `{ ratio = 0.5, min = 76 }` by default.
		picker_style = {
			type = "string",
			default = "comfortable",
			desc = "Default /resume look: a built-in one or a name from `picker_looks`.",
		},
		picker_looks = { type = "map", desc = "Looks to add or replace, by name." },
		picker_segments = { type = "map", desc = "Custom picker segments by name." },
		picker_split = { type = "map", desc = "The list's share of a wide picker next to its preview." },
	},
	apply = function(ctx, cfg)
		local S = {
			file = nil, -- store handle (created lazily on first message)
			path = nil,
			id = nil,
			entries = {}, -- tree id -> entry
			known_ids = {}, -- all entry ids (including non-tree records)
			list = {}, -- append order
			operations = {}, -- stable request_id -> committed generic operation receipt
			leaf = nil,
			name = nil,
			name_revision = 0,
			ids = setmetatable({}, { __mode = "k" }), -- message table -> entry id
		}
		local disabled = mare.args and mare.args.no_session or mare.mode == "child"
		local dir = mare.store.dir(mare.cwd)

		local session = {}
		local function warn_unsupported(paths)
			local new_warnings = mare.store.index_warn_unsupported(paths or {})
			if #new_warnings > 0 then
				ui.notify(
					"Skipped sessions with an unsupported version (use /delete <path> to delete):\n"
						.. table.concat(new_warnings, "\n"),
					"warning"
				)
			end
		end
		-- structural submodules (plugins/lua/session/), loaded here in dependency order
		local D = {
			ctx = ctx,
			cfg = cfg,
			S = S,
			disabled = disabled,
			-- A recorded child has one host-owned writer. Its session plugin stays
			-- ephemeral, but may read that writer's committed prefix for snapshots.
			recorded_path = mare.mode == "child"
					and not (mare.args and mare.args.no_session)
					and mare.args
					and mare.args.recorded_session_path
				or nil,
			dir = dir,
			session = session,
			warn_unsupported = warn_unsupported,
		}
		local mare_lib = require("mare_lib")
		-- optional services read on each call
		D.follow = {
			llm = mare_lib.follow(ctx, "llm"),
			compact = mare_lib.follow(ctx, "compact"),
			subagent = mare_lib.follow(ctx, "subagent"),
			changes = mare_lib.follow(ctx, "changes"),
		}
		-- session/loaded, then the recovery of a pending initial handoff (session.transitions sets
		-- D.after_loaded): after every listener of the event returned, as its emitter (it was the
		-- event's last listener, priority -100, before the tree)
		function D.emit_loaded(payload)
			if type(payload) == "table" and payload.session_id == nil and S.id ~= nil then
				payload.session_id = S.id
			end
			ctx.bus:emit("session/loaded", payload)
			if D.after_loaded then
				D.after_loaded()
			end
		end
		function D.check_expected_epoch(expected_epoch, target_sid)
			if expected_epoch == nil then
				return
			end
			local lib = require("mare_lib")
			if type(expected_epoch) ~= "string" or expected_epoch == "" then
				lib.fail("invalid_arguments", "expected_epoch must be a non-empty string")
			end
			local cur_epoch = ctx.kernel and ctx.kernel:session_epoch() or nil
			if (target_sid ~= nil and target_sid ~= S.id) or cur_epoch ~= expected_epoch then
				lib.fail("stale_session", "the session changed since expected_epoch was obtained")
			end
		end
		-- A resumed session may live under a different cwd's directory.
		D.child_dir = function()
			local recorded_path = type(D.recorded_path) == "string" and tostring(D.recorded_path) or nil
			if recorded_path then
				-- Keep descendants in the standard session/agents/run layout so the
				-- existing index and search discovery find every generation.
				local root = recorded_path:match("^(.*)[/\\]agents[/\\][^/\\]+[/\\][^/\\]+$")
				if root then
					return root
				end
			end
			local path = S.path or recorded_path or S.planned
			return path and path:match("^(.*)[/\\][^/\\]+$") or dir
		end
		-- Track and freeze active zero-copy baseline slices right before S.list is cleared or replaced.
		D._baseline_cuts = {}
		D.freeze_baseline_cuts = function()
			for cut in pairs(D._baseline_cuts) do
				if not cut.frozen then
					local frozen = {}
					local src = cut.list
					for i = 1, cut.count do
						frozen[i] = src[i]
					end
					cut.frozen = frozen
				end
			end
		end
		-- ctx:service can expose this implementation directly, without a service proxy.
		-- Refuse its mutating methods before they change in-memory state or policy.
		local poison_reason = mare.task.session_error
		local diagnostic = {
			switching = true,
			info = true,
			available = true,
			entries = true,
			path_to = true,
			leaf = true,
			entry_id = true,
			release_operation = true, -- lifecycle cleanup must wake duplicates even after publication poisoning
			list = true,
			get = true,
			list_items = true,
			list_records = true,
			children = true,
			file_changes = true,
			short_id = true,
			live_snapshot = true,
			baseline_cut = true,
		}
		local spi_raise = {
			persist_message = true,
			record_tool_state = true,
			record_request = true,
			record_request_end = true,
			record_request_aborted = true,
			switch = true,
			prepare_fork = true,
			snapshot = true,
			rewind = true,
			add_record = true,
			update = true,
			label_item = true,
			archive = true,
			unarchive = true,
			delete = true,
		}
		local function wrap_method(name, method)
			if type(method) ~= "function" or diagnostic[name] then
				return method
			end
			return function(...)
				local failure = poison_reason()
				if failure then
					if spi_raise[name] then
						mare_lib.fail("persist_failed", failure)
					end
					return nil, failure
				end
				return method(...)
			end
		end
		D.store = require("session.store")(D)
		local drains = {}
		local drain_seq = 0
		function session.on_drain(_, caller, fn)
			assert(type(fn) == "function", "session.on_drain expects a function")
			drain_seq = drain_seq + 1
			local id = drain_seq
			drains[id] = fn
			local release = function()
				drains[id] = nil
			end
			if caller and caller.effect then
				caller:effect(function()
					return release
				end, "session drain")
			end
			return release
		end
		function D.run_session_drains()
			local ids = {}
			for id in pairs(drains) do
				ids[#ids + 1] = id
			end
			table.sort(ids)
			local first_err
			for _, id in ipairs(ids) do
				local fn = drains[id]
				if fn then
					local ok, err = pcall(fn)
					if not ok and first_err == nil then
						first_err = err
					end
				end
			end
			if first_err ~= nil then
				error(first_err, 0)
			end
		end
		D.lazy_operations = true
		function D.on_dispose_operations() end
		D.operations = select(
			2,
			mare_lib.lazy_group(session, { "operation_receipt", "claim_operation", "release_operation" }, function()
				return require("session.operations")(D)
			end)
		)
		ctx:effect(function()
			return function()
				D.on_dispose_operations()
			end
		end, "session operation claims")
		local ensure_history = mare_lib.lazy(function()
			return require("session.history")(D)
		end)
		D.history = setmetatable({}, {
			__index = function(_, k)
				return ensure_history()[k]
			end,
		})
		function D.resolve_item_entry(source, item_id)
			ensure_history()
			return D.resolve_item_entry(source, item_id)
		end
		require("session.queries")(D)
		mare_lib.lazy_group(session, { "prepare_children", "child", "children", "load_transcript" }, function()
			return require("session.children")(D)
		end)
		mare_lib.lazy_group(session, { "append_compaction", "append_prune" }, function()
			return require("session.compaction")(D)
		end)
		D.fork = select(
			2,
			mare_lib.lazy_group(session, { "set_leaf", "prepare_fork" }, function()
				return require("session.fork")(D)
			end, {
				extra_target = D,
				extra_fields = {
					"move_leaf",
					"validate_fork_source",
					"prepare_fork",
					"completed_fork_boundary",
					"completed_fork_source",
					"read_validated_fork_snapshot",
					"hydrate_verified_fork_images",
				},
			})
		)
		D.fork_snapshot = select(
			2,
			mare_lib.lazy_group(session, { "snapshot_context", "snapshot" }, function()
				return require("session.fork_snapshot")(D)
			end)
		)
		D.lazy_recorder = true
		D.recorder_events = {
			on_step_started = function() end,
			on_turn_completed = function() end,
		}
		D.transitions_events = {
			on_turn_completed = function(_ev) end,
			on_agent_settled = function(_ev) end,
			on_session_changed = function() end,
		}
		local ensure_recorder
		ensure_recorder = mare_lib.lazy_group(session, {
			"add_record",
			"commit_step",
			"list_records",
			"persist_message",
			"record_request",
			"record_request_end",
			"record_request_aborted",
			"record_run_end",
			"commit",
		}, function()
			return require("session.recorder")(D)
		end, {
			extra_target = D,
			extra_fields = { "set_name" },
			fast_unloaded = {
				list_records = function(self, caller, kind, opts)
					if #S.list == 0 and type(kind) == "string" and kind ~= "" then
						if opts == nil then
							return { data = mare.json.array({}) }
						end
						if type(opts) == "table" then
							local simple = true
							for key, value in pairs(opts) do
								if
									not (
										(key == "branch" and (value == "current" or value == "all"))
										or (key == "limit" and type(value) == "number" and value >= 1 and value % 1 == 0)
										or (key == "loaded_only" and type(value) == "boolean")
									)
								then
									simple = false
									break
								end
							end
							if simple then
								return { data = mare.json.array({}) }
							end
						end
					end
					ensure_recorder()
					return session.list_records(self, caller, kind, opts)
				end,
			},
		})
		ctx:on("step/started", function()
			D.recorder_events.on_step_started()
		end)
		ctx:on("turn/completed", function(ev)
			D.recorder_events.on_turn_completed()
			D.transitions_events.on_turn_completed(ev)
		end)
		ctx:on("agent/settled", function(ev)
			D.transitions_events.on_agent_settled(ev)
		end)
		ctx:on("session/changed", function()
			D.transitions_events.on_session_changed()
		end)
		mare_lib.lazy_group(session, { "record_tool_state" }, function()
			return require("session.tool_state")(D)
		end)
		D.lifecycle = require("session.lifecycle")(D)
		D.lazy_transitions = true
		D.transitions = select(
			2,
			mare_lib.lazy_group(session, { "transition" }, function()
				return require("session.transitions")(D)
			end, {
				extra_target = D,
				extra_fields = { "after_loaded" },
			})
		)
		D.lazy_snapshots = true
		local ensure_snapshots, snapshots_proxy = mare_lib.lazy_group(session, { "file_changes" }, function()
			return require("session.snapshots")(D)
		end, {
			fast_unloaded = {
				file_changes = function()
					return {}
				end,
			},
		})
		D.snapshots = snapshots_proxy
		ctx.policy:voter("tool/before", function(_, info)
			if S.readonly then
				return require("session.readonly")().vote(info)
			end
			return nil
		end)
		ctx.policy:voter("tool/before", function(call)
			if not S.turn_id or not call or call.name == "read" or call.name == "grep" or call.name == "ls" then
				return nil
			end
			return ensure_snapshots().on_tool_before(call)
		end)
		D.pickers = select(
			2,
			mare_lib.lazy_group(session, { "short_id" }, function()
				return require("session.pickers")(D)
			end)
		)
		local rewind_mod
		D.rewind = setmetatable({}, {
			__index = function(_, key)
				if not rewind_mod then
					rewind_mod = require("session.rewind")(D)
				end
				return rewind_mod[key]
			end,
		})
		D.metadata = select(
			2,
			mare_lib.lazy_group(
				session,
				{ "update", "label_item", "archive", "unarchive", "delete", "rewind" },
				function()
					return require("session.metadata")(D)
				end
			)
		)
		require("session.startup")(D)
		for name, method in pairs(session) do
			session[name] = wrap_method(name, method)
		end

		-- the open session in this process's peer registry entry (`plugins.session`): another
		-- process's /resume can tell it is open here
		if mare.peers then
			local function publish()
				mare.peers.publish("session", { session_id = S.id, name = S.name })
			end
			for _, ev in ipairs({
				"session/started",
				"session/changed",
				"session/new",
				"session/loaded",
				"session/named",
			}) do
				ctx:on(ev, publish)
			end
			ctx:effect(function()
				return function()
					mare.peers.publish("session", nil)
				end
			end, "session peers publication")
		end

		-- registered while the optional `commands` service is up (a child fiber on the cordis tree)
		ctx:inject({ "commands" }, function(sub)
			-- its registrations and listeners belong to this follower (undone with `commands`),
			-- not to the session plugin: D with the follower's ctx
			require("session.commands")(setmetatable({ ctx = sub }, { __index = D }))
		end)
		-- a mare.Service
		ctx:on("kernel/before-reload", function(ev)
			if type(ev) == "table" and type(ev.plugins) == "table" then
				for _, name in ipairs(ev.plugins) do
					if name == PLUGIN_NAME then
						D.store.retire_on_shutdown("reload")
						return
					end
				end
			end
		end)
		ctx:on("app/shutdown", function()
			D.store.retire_on_shutdown("shutdown")
		end)
		D.lifecycle.reset()
		mare_lib.service(ctx, "session", session)
	end,
}
