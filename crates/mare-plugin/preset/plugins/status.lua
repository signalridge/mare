-- status: one agent state for everything that reports it — status line, notifications, terminal
-- title, RPC `status/changed`, and external tools via shell hooks. Built only from
-- public events: agent/*, ui/wait-*, jobs/changed, session/*, model/changed.
--
--   state = { state = "idle"|"working"|"blocked", message, session = { session_id, path, resume },
--             cwd, model, pid, seq }
--
-- Events (emit): status/changed {state…}, status/session {state…}, status/exit {state…}
--
-- init.lua:
--   mare.status.hook { on = { "status", "session", "exit" }, cmd = { … } | function(s) -> argv,
--                      throttle_ms = 200, timeout_ms = 2000 }
-- The command runs in the background with the state as JSON on stdin and MARE_STATE,
-- MARE_MESSAGE, MARE_SESSION_ID, MARE_SESSION_PATH, MARE_CWD, MARE_SESSION_MODEL in its environment.

local function summarize(call)
	if type(call) ~= "table" then
		return nil
	end
	local a = type(call.arguments) == "table" and call.arguments or {}
	local detail = a.description or a.command or a.path or a.file_path or a.pattern or a.query or a.url
	if detail == nil then
		for _, v in pairs(a) do
			if type(v) == "string" then
				detail = v
				break
			end
		end
	end
	local name = call.name or "tool"
	if type(detail) ~= "string" or detail == "" then
		return name
	end
	return name .. ": " .. detail
end

local function one_line(s, max)
	s = tostring(s or ""):gsub("%s+", " "):gsub("^ ", ""):gsub(" $", "")
	-- display width, never splitting a UTF-8 sequence (CJK, emoji)
	return mare.text.trunc_width(s, max)
end

local VALID_HOOK_ON = { status = true, session = true, exit = true }

return {
	name = "status",
	provide = { "status" },
	-- `task` gives ctx:spawn, `bus` the mare events; llm is
	-- followed below
	inject = { "task", "bus" },
	config_api = {
		--- mare.status.hook { on = {...}, cmd = argv | fn(state) -> argv, throttle_ms, timeout_ms }
		["status.hook"] = function(C, def)
			assert(type(def) == "table" and def.cmd, "mare.status.hook needs `cmd`")
			if def.on ~= nil then
				local list = type(def.on) == "table" and def.on or { def.on }
				for _, k in ipairs(list) do
					if not VALID_HOOK_ON[k] then
						local hint = k == "changed" and ' (use "status")' or ""
						error(
							('mare.status.hook: unknown `on` event %q%s; expected "status", "session" or "exit"'):format(
								tostring(k),
								hint
							),
							0
						)
					end
				end
			end
			-- copy: the caller's table stays untouched (a function `cmd` survives config layering)
			local entry = {}
			for k, v in pairs(def) do
				entry[k] = v
			end
			C.status_hooks = C.status_hooks or {}
			table.insert(C.status_hooks, entry)
		end,
	},
	config_schema = {
		throttle_ms = { type = "integer", default = 200, desc = "Default per-hook coalescing window." },
		timeout_ms = {
			type = "integer",
			default = 2000,
			desc = "Default per-hook command timeout (the process is killed after it).",
		},
		exit_timeout_ms = {
			type = "integer",
			default = 3000,
			desc = "How long shutdown waits for `exit` hooks in total.",
		},
		message_max = { type = "integer", default = 120, desc = "Message length cap." },
		title = { type = "boolean", default = true, desc = "Set the terminal title (OSC 2) from the state." },
		title_format = {
			type = "string",
			default = "%s — mare",
			desc = 'Terminal title; %s = "working: …" / "waiting: …" / project name.',
		},
	},
	apply = function(ctx, cfg)
		local turn = false
		local tool -- current tool call summary
		local waits = {} -- stack of {kind, title}
		local jobs = {} -- id -> label (running only)
		local session = { id = nil, path = nil }
		local model
		local seq = 0
		local cur -- last published state
		local exited = false

		local function next_seq()
			-- strictly increasing; epoch-based so it also orders across restarts
			local now = math.floor(mare.task.now() * 1000000)
			seq = math.max(seq + 1, now)
			return seq
		end

		local function job_summary()
			local n, label = 0, nil
			for _, l in pairs(jobs) do
				n = n + 1
				label = label or l
			end
			if n == 0 then
				return nil
			end
			return n == 1 and ("background: " .. tostring(label)) or ("%d background jobs"):format(n)
		end

		-- sub-agents (mare.subagent: agent/child-start/end) and workflow runs (workflow/* events)
		-- also mean working
		local children = {} -- id -> label
		local workflows = {} -- run id -> {done, total, name}
		local function child_summary()
			local n, label = 0, nil
			for _, l in pairs(children) do
				n = n + 1
				label = label or l
			end
			if n == 0 then
				return nil
			end
			return n == 1 and ("sub-agent" .. (label and (": " .. label) or "")) or (n .. " sub-agents")
		end
		local function workflow_summary()
			for _, w in pairs(workflows) do
				if w.total and w.total > 0 then
					return ("workflow %d/%d"):format(w.done or 0, w.total)
				end
				return "workflow" .. (w.name and (" " .. w.name) or "")
			end
		end
		local function compute()
			local state, message
			if #waits > 0 then
				local w = waits[#waits]
				state = "blocked"
				message = "waiting: " .. tostring((w.title ~= nil and w.title ~= "") and w.title or w.kind or "input")
			elseif turn or next(jobs) or next(children) or next(workflows) then
				state = "working"
				message = (turn and (tool or "thinking")) or workflow_summary() or child_summary() or job_summary()
			else
				state = "idle"
				message = ""
			end
			message = one_line(message, cfg.message_max)
			local s = {
				state = state,
				message = message,
				session = {
					session_id = session.id,
					path = session.path,
					resume = session.id and { "mare", "-r", session.id } or nil,
				},
				cwd = mare.cwd,
				model = model,
				pid = mare.pid,
			}
			return s
		end

		-------------------------------------------------------------------------
		-- shell hooks
		-------------------------------------------------------------------------
		local hook_state = setmetatable({}, { __mode = "k" }) -- def -> {last, timer, pending, key}

		local function hooks_for(kind)
			local out = {}
			for _, h in ipairs((mare.config and mare.config.status_hooks) or {}) do
				local on = h.on or { "status" }
				for _, k in ipairs(type(on) == "table" and on or { on }) do
					if k == kind then
						out[#out + 1] = h
						break
					end
				end
			end
			return out
		end

		local function run_hook(h, s)
			local argv = h.cmd
			if type(argv) == "function" then
				local ok, r = pcall(argv, s)
				if not ok then
					ctx.logger:warn("status hook: " .. tostring(r))
					return
				end
				argv = r
			end
			if type(argv) ~= "table" or not argv[1] then
				return
			end
			local args = {}
			for i = 2, #argv do
				args[#args + 1] = tostring(argv[i])
			end
			local r = mare.proc.run({
				cmd = tostring(argv[1]),
				args = args,
				input = mare.json.encode(s),
				timeout_ms = h.timeout_ms or cfg.timeout_ms,
				env = {
					MARE_STATE = s.state,
					MARE_MESSAGE = s.message or "",
					MARE_SESSION_ID = s.session.session_id or "",
					MARE_SESSION_PATH = s.session.path or "",
					MARE_CWD = s.cwd or "",
					MARE_SESSION_MODEL = s.model or "",
					-- hooks may `mare peer send` back to this session
					MARE_PEER_SOCKET = (mare.peers and mare.peers.self() or {}).socket,
				},
			})
			if r.timed_out then
				ctx.logger:warn("status hook timed out: " .. tostring(argv[1]))
			elseif r.code ~= 0 then
				ctx.logger:warn(("status hook %s exited %s: %s"):format(argv[1], tostring(r.code), r.stderr or ""))
			end
		end

		local function hook_key(kind, s)
			if kind == "session" then
				local sess = type(s.session) == "table" and s.session or {}
				return table.concat({
					tostring(sess.session_id or ""),
					tostring(sess.path or ""),
					tostring(s.cwd or ""),
					tostring(s.model or ""),
				}, "\0")
			end
			return (s.state or "") .. "\0" .. (s.message or "")
		end

		--- fire a hook kind; "status" and "session" hooks are throttled + coalesced (only the
		--- latest snapshot runs) and skipped when the relevant fields did not change
		local function fire(kind, s)
			for _, h in ipairs(hooks_for(kind)) do
				if kind ~= "status" and kind ~= "session" then
					ctx:spawn(run_hook, h, s)
				else
					local by_kind = hook_state[h]
					if not by_kind then
						by_kind = {}
						hook_state[h] = by_kind
					end
					local st = by_kind[kind] or { last = -math.huge }
					by_kind[kind] = st
					local key = hook_key(kind, s)
					if key ~= st.key then
						st.key = key
						st.pending = s
						if not st.timer then
							st.timer = true
							-- one run of a hook at a time: a change that arrives while it runs waits
							-- for it, so the hook receives updates in order and the last one it gets
							-- is the latest (overlapping runs could finish in either order)
							ctx:spawn(function()
								while st.pending do
									local wait = (st.last + (h.throttle_ms or cfg.throttle_ms)) - mare.task.clock()
									if wait > 0 then
										mare.task.sleep(wait)
									end
									local latest = st.pending
									st.pending = nil
									-- compare with what the hook last *received*: a burst that ends
									-- where it started delivers nothing new
									local lkey = hook_key(kind, latest)
									if lkey ~= st.delivered then
										st.delivered, st.last = lkey, mare.task.clock()
										local ok, err = pcall(run_hook, h, latest)
										if not ok then
											ctx.logger:warn("status hook: " .. tostring(err))
										end
									end
								end
								st.timer = nil
							end)
						end
					end
				end
			end
		end

		-------------------------------------------------------------------------
		-- publishing
		-------------------------------------------------------------------------
		local function set_title(s)
			if not cfg.title or mare.mode ~= "tui" or not mare.ui.set_title then
				return
			end
			local label
			if s.state == "idle" then
				label = (mare.cwd or ""):match("([^/]+)/*$") or "mare"
			else
				label = s.message ~= "" and s.message or s.state
			end
			pcall(mare.ui.set_title, cfg.title_format:format(label))
		end

		local function publish()
			if exited then
				return
			end
			local s = compute()
			if cur and cur.state == s.state and cur.message == s.message and cur.model == s.model then
				return
			end
			s.seq = next_seq()
			cur = s
			set_title(s)
			ctx.bus:emit("status/changed", s)
			fire("status", s)
		end

		local function publish_session()
			local s = compute()
			s.seq = next_seq()
			cur = s
			ctx.bus:emit("status/session", s)
			fire("session", s)
		end

		-------------------------------------------------------------------------
		-- inputs
		-------------------------------------------------------------------------
		ctx:on("turn/started", function()
			turn, tool = true, nil
			publish()
		end)
		ctx:on("item/started", function(ev)
			local item = type(ev) == "table" and ev.item
			if item and item.type == "tool_call" then
				tool = summarize(item)
				publish()
			end
		end)
		ctx:on("item/completed", function(ev)
			local item = type(ev) == "table" and ev.item
			if item and item.type == "tool_call" then
				tool = nil
				publish()
			end
		end)
		ctx:on("turn/completed", function()
			turn, tool = false, nil
			publish()
		end)
		ctx:on("ui/wait-started", function(ev)
			table.insert(waits, { kind = ev and ev.kind, title = ev and ev.title, wait_id = ev and ev.wait_id })
			publish()
		end)
		ctx:on("ui/wait-completed", function(ev)
			-- close this wait, not whichever opened last (two can overlap); a completion without
			-- a wait_id closes the newest
			local wait_id = type(ev) == "table" and ev.wait_id or nil
			local at = #waits
			if wait_id ~= nil then
				at = nil
				for i = #waits, 1, -1 do
					if waits[i].wait_id == wait_id then
						at = i
						break
					end
				end
			end
			if at then
				table.remove(waits, at)
			end
			publish()
		end)
		-- inbound peer messages waiting for this user's answer, in the peer registry entry
		-- each held message ends released or rejected
		if mare.peers then
			local held = 0
			local function set_held(n)
				held = math.max(n, 0)
				mare.peers.publish("status", { held = held })
			end
			set_held(0)
			ctx:on("peer/held", function()
				set_held(held + 1)
			end)
			for _, ev in ipairs({ "peer/released", "peer/rejected" }) do
				ctx:on(ev, function()
					set_held(held - 1)
				end)
			end
			ctx:effect(function()
				return function()
					mare.peers.publish("status", nil)
				end
			end, "status peer publication")
		end
		ctx:on("subagent/started", function(ev)
			if type(ev) == "table" and ev.subagent_id then
				children[ev.subagent_id] = ev.label or false
				publish()
			end
		end)
		ctx:on("subagent/completed", function(ev)
			if type(ev) == "table" and ev.subagent_id then
				children[ev.subagent_id] = nil
				publish()
			end
		end)
		local function wf_key(ev)
			return type(ev) == "table" and (ev.run_id or "workflow") or "workflow"
		end
		ctx:on("workflow/started", function(ev)
			local k = wf_key(ev)
			workflows[k] = {
				done = 0,
				total = type(ev) == "table" and tonumber(ev.total) or nil,
				name = type(ev) == "table" and ev.name or nil,
			}
			publish()
		end)
		ctx:on("workflow/agent-started", function(ev)
			local w = workflows[wf_key(ev)]
			if w and type(ev) == "table" and tonumber(ev.total) then
				w.total = tonumber(ev.total)
			end
			publish()
		end)
		ctx:on("workflow/agent-completed", function(ev)
			local k = wf_key(ev)
			local w = workflows[k] or { done = 0 }
			workflows[k] = w
			w.done = (w.done or 0) + 1
			if type(ev) == "table" and tonumber(ev.total) then
				w.total = tonumber(ev.total)
			end
			publish()
		end)
		ctx:on("workflow/completed", function(ev)
			workflows[wf_key(ev)] = nil
			publish()
		end)
		ctx:on("jobs/changed", function(ev)
			local job = ev and ev.job
			if not job or not job.job_id then
				return
			end
			-- foreground commands only run inside a turn, so counting every running job is right
			if job.status == "running" then
				jobs[job.job_id] = job.label or job.description or job.kind or job.job_id
			else
				jobs[job.job_id] = nil
			end
			publish()
		end)
		local function on_session(ev)
			if ev and ev.session_id then
				session.id, session.path = ev.session_id, ev.path or session.path
				publish_session()
			end
		end
		ctx:on("session/started", on_session)
		ctx:on("session/changed", on_session)
		ctx:on("model/changed", function(ev)
			model = ev and ev.ref or model
			publish()
		end)

		-- `exit` always fires on a clean shutdown (Host::shutdown emits app/shutdown on /quit,
		-- Ctrl+D, double Ctrl+C, headless end, SIGTERM/SIGHUP) and shutdown waits for the hooks
		ctx:on("app/shutdown", function()
			if exited then
				return
			end
			local s = compute()
			s.state, s.message = "idle", ""
			s.seq = next_seq()
			exited = true
			ctx.bus:emit("status/exit", s)
			local tasks = {}
			for _, h in ipairs(hooks_for("exit")) do
				tasks[#tasks + 1] = ctx:spawn(run_hook, h, s)
			end
			if #tasks > 0 then
				mare.task.timeout(cfg.exit_timeout_ms, function()
					for _, t in ipairs(tasks) do
						pcall(t.wait, t)
					end
				end)
			end
		end)

		-- registered while the optional `llm` service is up (a child fiber on the cordis tree)
		ctx:inject({ "llm" }, function(ctx)
			local ok, m = pcall(function()
				return ctx.llm:current()
			end)
			if ok and type(m) == "table" then
				model = m.ref
			end
		end)

		-- a mare.Service
		require("mare_lib").service(ctx, "status", {
			--- get() -> the current state
			get = function()
				return cur or compute()
			end,
		})
		publish()
	end,
}
