-- notify: terminal notification (OSC 9 / OSC 777 / kitty OSC 99, tmux & screen passthrough, or a
-- plain bell) when mare needs you: a turn that took a while has finished, or a picker / prompt is
-- waiting. Quick turns don't notify; only while the terminal is
-- unfocused (a terminal that never reports focus counts as unknown, and notifies); repeats within
-- `dedupe_ms` are dropped. Turn it off with mare.plugin("notify", {enabled=false}).
--
-- `cmd`: an external program run when a turn finishes or mare waits for
-- you, in every mode (TUI, headless, RPC), with one JSON object on stdin:
--   {"type": "agent-turn-complete" | "input-required", "session-id", "turn-id", "cwd",
--    "last-assistant-message" (turn complete), "message" (input required), "error"?}
-- e.g. mare.plugin("notify", { cmd = { "notify-send-mare" } }). Its output is discarded; it gets
-- `cmd_timeout_ms` before it is stopped.

return {
	name = "notify",
	-- `task` gives ctx:spawn, status and session are read when used
	inject = { "task" },
	config_schema = {
		on_done = { type = "boolean", default = true, desc = "Notify when a turn is done." },
		on_input = { type = "boolean", default = true, desc = "Notify when input is needed." },
		only_unfocused = { type = "boolean", default = true, desc = "Notify only while the terminal is unfocused." },
		min_turn_ms = { type = "integer", default = 5000, desc = "Shortest turn that notifies." },
		-- "auto" picks "native" on Windows outside Windows Terminal, where OSC notifications don't show
		protocol = {
			-- a string, not an enum: the frontends' handling (mare-tui out.rs) is looser than the
			-- documented values, so they are not a closed set yet
			type = "string",
			default = "auto",
			desc = 'Notification protocol; "bel" is just the bell, "native" a desktop notification.',
		},
		bell = { type = "boolean", default = false, desc = "Also ring the bell after the OSC notification." },
		dedupe_ms = {
			type = "integer",
			default = 10000,
			desc = "Window in which an identical notification is dropped.",
		},
		cmd = {
			type = { "string", "table" },
			desc = "Program (string) or argv list run with the event as JSON on stdin.",
		},
		cmd_timeout_ms = { type = "integer", default = 10000, desc = "Timeout of `cmd` in milliseconds." },
	},
	apply = function(ctx, cfg)
		local tui = mare.mode == "tui" and mare.ui.notify_os ~= nil

		-- ---- notify.cmd ------------------------------------------------------------------------
		local argv
		if type(cfg.cmd) == "string" and cfg.cmd ~= "" then
			argv = { cfg.cmd }
		elseif type(cfg.cmd) == "table" and type(cfg.cmd[1]) == "string" then
			argv = cfg.cmd
		end
		local function session_id()
			local session = ctx:get("session")
			if session and session.info then
				local ok, info = pcall(session.info, session)
				return ok and type(info) == "table" and info.session_id or nil
			end
		end
		local function run_cmd(payload)
			if not argv then
				return
			end
			payload["session-id"] = session_id()
			payload.cwd = mare.cwd
			local args = {}
			for i = 2, #argv do
				args[#args + 1] = argv[i]
			end
			ctx:spawn(function()
				local ok, h = pcall(mare.proc.start, {
					cmd = argv[1],
					args = args,
					input = mare.json.encode(payload),
					buffer_bytes = 4096,
				})
				if not ok then
					mare.log("warn", "notify.cmd: " .. tostring(h))
					return
				end
				local st = h:wait({ timeout_ms = cfg.cmd_timeout_ms })
				if st.state ~= "exited" then
					pcall(h.stop, h, { grace_ms = 500 })
				end
			end)
		end

		-- ---- terminal notification -------------------------------------------------------------
		local last_text, last_at = nil, -math.huge
		local function send(text)
			if not tui then
				return
			end
			local now = mare.task.clock()
			if text == last_text and now - last_at < (cfg.dedupe_ms or 0) then
				return
			end
			local ok, sent = pcall(mare.ui.notify_os, text, {
				only_unfocused = cfg.only_unfocused ~= false,
				protocol = cfg.protocol,
				bell = cfg.bell == true,
			})
			if ok and sent then
				last_text, last_at = text, now
			end
		end

		if cfg.on_done then
			local started, turn_id = 0, nil
			ctx:on("turn/started", function(ev)
				started = mare.task.clock()
				turn_id = type(ev) == "table" and ev.turn_id or nil
			end)
			ctx:on("turn/completed", function(res)
				if res and (res.status == "cancelled" or res.termination_reason == "interrupted") then
					return
				end
				local err_text = res and require("mare_lib").error_message(res.error) or nil
				run_cmd({
					type = "agent-turn-complete",
					["turn-id"] = turn_id,
					["last-assistant-message"] = res and res.text or nil,
					error = err_text,
				})
				if mare.task.clock() - started < (cfg.min_turn_ms or 0) and not err_text then
					return
				end
				send(err_text and ("error — " .. err_text) or "done")
			end)
		end

		local function waiting(text)
			send(text)
			run_cmd({ type = "input-required", message = text })
		end

		if cfg.on_input then
			local status_active = false
			-- follow `status` whenever it loads (independent of plugin start order or /reload)
			ctx:inject({ "status" }, function(sub)
				status_active = true
				local blocked = false
				sub:on("status/changed", function(s)
					if s.state == "blocked" then
						local text = (s.message ~= "" and s.message) or "waiting for input"
						send(text)
						-- the external program runs once per wait (status/changed repeats while it lasts)
						if not blocked then
							blocked = true
							run_cmd({ type = "input-required", message = text })
						end
					else
						blocked = false
					end
				end)
				sub:effect(function()
					return function()
						status_active = false
					end
				end, "notify status flag")
			end)
			if tui then
				-- fallback when the status plugin is not loaded: wrap the replaceable UI functions
				local wrapped = {}
				for _, name in ipairs({ "select", "input" }) do
					local orig = mare.ui[name]
					local function w(t, ...)
						if not status_active then
							waiting((type(t) == "table" and t.title) or "waiting for input")
						end
						return orig(t, ...)
					end
					wrapped[name] = { orig = orig, w = w }
					mare.ui[name] = w
				end
				ctx:effect(function()
					return function()
						for name, x in pairs(wrapped) do
							if mare.ui[name] == x.w then
								mare.ui[name] = x.orig
							end
						end
					end
				end, "notify ui wrappers")
			end
		end
	end,
}
