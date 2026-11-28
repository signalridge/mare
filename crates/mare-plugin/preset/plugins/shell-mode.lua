-- shell-mode: `!cmd` runs a shell command and puts command + output into the context (the model
-- sees it on the next turn); `!!cmd` runs it without adding anything to the context.
-- Uses the same shell, prelude and MARE_* environment as the bash tool.

return {
	name = "shell-mode",
	inject = { "tools", "task" },
	config_schema = {
		timeout_ms = { type = "integer", default = 600000, desc = "Timeout of a `!cmd` command." },
		-- what the model sees of the output (middle-truncated beyond this)
		context_max_lines = {
			type = "integer",
			default = 2000,
			desc = "Lines of output the model sees (middle-truncated beyond this).",
		},
		context_max_bytes = {
			type = "integer",
			default = 50 * 1024,
			desc = "Bytes of output the model sees (middle-truncated beyond this).",
		},
		-- collapsed transcript preview; the expanded view (Ctrl+O) shows retained output
		preview_lines = {
			type = "integer",
			default = 5,
			desc = "Lines of the collapsed transcript preview; Ctrl+O expands retained output.",
		},
		expanded_max_bytes = {
			type = "integer",
			default = 64 * 1024,
			desc = "Maximum inline bytes kept per expanded shell-mode block before spilling full output to disk.",
		},
		retained_blocks = {
			type = "integer",
			default = 20,
			desc = "Recent large shell-mode blocks kept inline-expandable in memory before spilling older blocks.",
		},
	},
	apply = function(ctx, cfg)
		-- the optional services, read on each call
		local svc = require("mare_lib").followers(ctx, { "agent", "session" })
		local U = mare.ui

		local function session_identity()
			local s = svc.session
			if not (s and s.info) then
				return nil, nil
			end
			local ok, info = pcall(s.info, s)
			if not ok or type(info) ~= "table" then
				return nil, nil
			end
			return info.session_id, info.session_epoch
		end

		-- `!` / `!!` on an empty input enter shell mode only while this plugin says so; the look
		-- is ours too (the core just draws it)
		U.editor_config({
			shell = {
				placeholder = "Shell command… (Esc to leave shell mode)",
				private_placeholder = "Shell command, output kept out of the context… (Esc to leave)",
				border = "MareAccent",
			},
		})
		ctx:effect(function()
			return function()
				U.editor_config({ shell = false })
			end
		end, "shell-mode editor")

		-- the running `!cmd`s: Esc / Ctrl+C abort them — SIGINT to
		-- the process group, then SIGTERM, then SIGKILL; a second Ctrl+C forces the kill
		local running = {}
		local retained = {}
		ctx:on("session/changed", function()
			for tok in pairs(running) do
				tok:cancel()
			end
			for i = #retained, 1, -1 do
				retained[i] = nil
			end
		end)
		-- when the last command ended: the interrupt that stopped it can reach this handler after
		-- the process is already gone (another path signalled it first), and must still be consumed
		local last_end = -math.huge
		local submit
		-- steps of ui's ordered interrupt and submit chains, registered while the ui service is up
		ctx:inject({ "ui" }, function(sub)
			sub.ui:handler("interrupt", "shell-mode", { order = 10 }, function()
				local any = mare.task.clock() - last_end < 500
				for tok in pairs(running) do
					tok:cancel()
					any = true
				end
				-- taken: not an agent interrupt too
				return any
			end)
			sub.ui:handler("submit", "shell-mode", { order = 30 }, function(ev)
				return submit(ev)
			end)
		end)

		submit = function(ev)
			local text = ev.text or ""
			local private, cmd = false, string.match(text, "^!!%s*(.+)$")
			if cmd then
				private = true
			else
				cmd = string.match(text, "^!%s*(.+)$")
			end
			if not cmd then
				return false
			end
			local tok = mare.task.token()
			local start_sid, start_epoch = session_identity()
			running[tok] = true
			ctx:spawn(function()
				local run, show = require("shell-mode.execute")(ctx, cfg, running, function()
					last_end = mare.task.clock()
				end, retained)
				local agent = svc.agent
				if U.connected() and not (agent and agent:activity().busy) then
					U.working("Running " .. mare.text.trunc_width(cmd, 40))
				end
				local out, code = run(cmd, tok)
				agent = svc.agent
				if U.connected() and next(running) == nil and not (agent and agent:activity().busy) then
					U.working(nil)
				end
				local cur_sid, cur_epoch = session_identity()
				if start_sid ~= cur_sid or start_epoch ~= cur_epoch then
					return
				end
				show(cmd, out, code, private)
				if private or not agent then
					return
				end
				local body = mare.text.truncate(
					out,
					{ max_lines = cfg.context_max_lines, max_bytes = cfg.context_max_bytes, keep = "middle" }
				)
				local xml = ('<bash command="%s" exit_code="%d">\n%s\n</bash>'):format(
					(string.gsub(cmd, '"', "&quot;")),
					code,
					body
				)
				if agent:activity().busy then
					-- mid-run: deliver after the current tool results, like a steer message
					agent:steer(xml)
				else
					agent:append({ role = "user", shell = true, content = { { type = "text", text = xml } } })
				end
			end)
			return true
		end
	end,
}
