-- tools.bash_run: running bash commands (direct, as jobs, in the persistent shell), the
-- carried-over working directory and Ctrl+B detaching; part of the `tools` plugin (plugins/tools.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local MAX_LINES, MAX_BYTES = cfg.max_lines, cfg.max_bytes
	local tools = D.registry.tools
	local new_spill, clean_output, bash_result =
		D.bash_output.new_spill, D.bash_output.clean_output, D.bash_output.bash_result
	local input_prompt_hint, partial_note = D.bash_output.input_prompt_hint, D.bash_output.partial_note
	local SPILL = D.bash_output.spill_prefix
	local function sandbox_report(handle)
		if not handle or not handle.sandbox then
			return nil
		end
		local ok, report = pcall(handle.sandbox, handle)
		return ok and report or nil
	end

	-- the bash tool's carried-over working directory (see bash_with_cwd below)
	local start_cwd = mare.cwd
	local shell_cwd = D.initial_shell_cwd or mare.cwd
	local reset_shell -- the persistent shell (see tools.bash_persistent)
	-- Ctrl+B (`background` action): every foreground bash
	-- command running now moves to the background; each waiting loop compares the generation
	-- it started with; returns false when idle so editor cursor-left stays active.
	local detach_gen, fg_commands = 0, 0
	local function detach_background()
		if fg_commands == 0 then
			return false
		end
		detach_gen = detach_gen + 1
	end
	if D.lazy_bash_run then
		D.detach_background = detach_background
	else
		ctx:inject({ "ui" }, function(ui_ctx)
			ui_ctx.ui:action("background", detach_background, {
				desc = "move the running command to the background",
				contexts = { "editor" },
				keys = { editor = "<C-b>" },
			})
		end)
	end
	local function validate_timeout(a)
		if
			a.timeout_ms ~= nil
			and (
				type(a.timeout_ms) ~= "number"
				or a.timeout_ms < 1
				or a.timeout_ms > cfg.bash_max_timeout_ms
				or math.floor(a.timeout_ms) ~= a.timeout_ms
			)
		then
			return ("timeout_ms must be an integer between 1 and %d"):format(cfg.bash_max_timeout_ms)
		end
		return nil
	end
	--- bash without the jobs plugin: run to completion (or timeout) with mare.proc.run
	local function bash_direct(a, t, command_of)
		local terr = validate_timeout(a)
		if terr then
			return { content = terr, is_error = true }
		end
		if a.run_in_background == true or a.stdin == true or a.tty == true then
			return {
				content = "Background jobs, stdin, and tty are unavailable because the jobs service is not loaded.",
				is_error = true,
			}
		end
		local tail = "" -- last few KB only, for the live view (full output comes from proc.run)
		local last = 0
		local prog, args, input = (command_of or function(c)
			return tools.shell_command(tools, ctx, c)
		end)(a.command)
		-- the complete output streams to a spill file while the command runs (the in-memory
		-- buffer keeps only the tail when output exceeds its cap)
		local spill = new_spill()
		local timeout_ms = math.min(a.timeout_ms or cfg.bash_timeout_ms, cfg.bash_max_timeout_ms)
		local started = mare.task.clock()
		local r = mare.proc.run({
			cmd = prog,
			args = args,
			input = input,
			stderr = "merge",
			spill = spill,
			spill_prefix = SPILL,
			timeout_ms = timeout_ms,
			cancel = t.cancel,
			cwd = a._cwd,
			env = tools.process_env(tools, ctx),
			env_include = tools.env_policy(tools, ctx).env_include,
			env_exclude = tools.env_policy(tools, ctx).env_exclude,
			on_output = function(chunk)
				tail = tail .. chunk
				if #tail > 2 * cfg.live_tail_bytes then
					tail = string.sub(tail, -cfg.live_tail_bytes)
				end
				local now = mare.task.clock()
				if t.update and now - last > cfg.update_interval_ms then
					last = now
					t.update({ output = tail })
				end
			end,
		})
		local notes = {}
		if r.timed_out then
			-- say what the limit was and how long it ran
			notes[#notes + 1] = ("[command timed out after %.1fs (timeout %.1fs)]"):format(
				(mare.task.clock() - started) / 1000,
				timeout_ms / 1000
			)
		end
		if r.cancelled then
			notes[#notes + 1] = "[command aborted]"
		end
		local bg_job_id
		if r.background_left then
			-- the shell exited but something it started still holds the output
			local J = D.svc.jobs
			if r.background_job and J and J.track then
				local desc = a.description
				if not desc or desc == "" then
					desc = mare.text.trunc_width((string.gsub(a.command or "", "%s+", " ")), 60)
				end
				local ok, job = pcall(J.track, J, {
					kind = "bash",
					description = desc,
					handle = r.background_job,
					background = true,
				})
				if ok and job and job.job_id then
					bg_job_id = job.job_id
				end
			end
			if bg_job_id then
				notes[#notes + 1] = ("[the command exited; background processes it started are still running as background job %s — check with job_output or stop with job_kill]"):format(
					bg_job_id
				)
			else
				notes[#notes + 1] = "[the command exited; background processes it started are still running]"
			end
		end
		if r.stop_unconfirmed then
			-- never claim the tree was cleaned up when it wasn't confirmed
			notes[#notes + 1] = "[stop_unconfirmed: the process tree could not be confirmed stopped]"
		end
		local out, binary = clean_output(r.stdout or "")
		return bash_result(a, {
			out = out,
			binary = binary,
			dropped = r.dropped,
			code = r.code,
			notes = notes,
			failed = r.timed_out or r.cancelled,
			job_id = bg_job_id,
			denial = r.denial,
			sandbox = r.sandbox,
			program = prog,
		}, spill)
	end

	--- bash through the jobs plugin: every command is a job; run_in_background
	--- returns at once; a foreground command that outlives its timeout moves to the background
	--- instead of being killed.
	local function bash_job(J, a, t, command_of)
		local terr = validate_timeout(a)
		if terr then
			return { content = terr, is_error = true }
		end
		local prog, args, input = (command_of or function(c)
			return tools.shell_command(tools, ctx, c)
		end)(a.command)
		local spill = new_spill()
		local desc = a.description
		if not desc or desc == "" then
			desc = mare.text.trunc_width((string.gsub(a.command or "", "%s+", " ")), 60)
		end
		local job, err = J:start({
			kind = "bash",
			description = desc,
			cmd = prog,
			args = args,
			stdin = a.stdin == true or a.tty == true,
			input = input,
			tty = a.tty == true,
			cwd = a._cwd,
			env = tools.process_env(tools, ctx),
			env_include = tools.env_policy(tools, ctx).env_include,
			env_exclude = tools.env_policy(tools, ctx).env_exclude,
			spill = spill,
			spill_prefix = SPILL,
			background = a.run_in_background == true,
		})
		if not job then
			return { content = "Could not start the command: " .. tostring(err), is_error = true }
		end
		if a.run_in_background then
			local report = sandbox_report(job.handle)
			local partial = partial_note(report)
			return {
				content = ("Started background job %s. You will be notified when it finishes; "):format(job.job_id)
					.. ("read its output any time with job_output(%q)."):format(job.job_id)
					.. (partial and ("\n" .. partial) or ""),
				details = { job_id = job.job_id, sandbox = report },
			}
		end
		local h = job.handle
		local limit = math.min(a.timeout_ms or cfg.bash_timeout_ms, cfg.bash_max_timeout_ms)
		local t0 = mare.task.clock()
		local st
		-- silence (`stall_ms`) and Ctrl+B move it to the background too, like the persistent
		-- shell
		local last_out, last_bytes, gen0 = t0, 0, detach_gen
		local why -- "stalled" | "detached" | nil (timeout)
		fg_commands = fg_commands + 1
		local _fg_guard <close> = require("mare_lib").guard(function()
			fg_commands = fg_commands - 1
		end)
		while true do
			local left = limit - (mare.task.clock() - t0)
			if left <= 0 then
				break
			end
			st = h:wait({ timeout_ms = math.min(150, left), cancel = t.cancel })
			if t.update then
				t.update({ output = h:tail(cfg.live_tail_bytes) })
			end
			if st.state == "exited" or st.cancelled or st.background_left then
				break
			end
			local now = mare.task.clock()
			if (st.bytes or 0) ~= last_bytes then
				last_bytes, last_out = st.bytes or 0, now
			end
			if detach_gen ~= gen0 then
				why = "detached"
				break
			end
			if cfg.stall_ms > 0 and now - last_out >= cfg.stall_ms then
				why = "stalled"
				break
			end
		end
		_fg_guard:release()
		if st and st.cancelled and st.state ~= "exited" then
			-- the tool call itself was cancelled (Esc): the command goes with it
			J:cancel(job.job_id, "cancelled")
			local r = h:read({ max_bytes = cfg.bash_read_bytes })
			local out, binary = clean_output(r.text or "")
			return bash_result(a, {
				out = out,
				binary = binary,
				dropped = r.dropped,
				notes = { "[command aborted]" },
				failed = true,
				job_id = job.job_id,
			}, spill)
		end
		if not st or st.state ~= "exited" then
			-- outlived the timeout, went silent or was sent away: keep it running as a
			-- background job
			J:background(job.job_id)
			local r = h:read({ max_bytes = cfg.bash_read_bytes })
			local out = clean_output(r.text or "")
			local head
			if why == "detached" then
				head = ("[moved to background job %s by the user — check it with job_output]"):format(job.job_id)
			elseif st and st.background_left then
				head = ("[the command exited; background processes it started are still running as background job %s — check with job_output or stop with job_kill]"):format(
					job.job_id
				)
			elseif why == "stalled" then
				head = ("[no output for %ds; still running as background job %s — check it with job_output, or stop it with job_kill]"):format(
					cfg.stall_ms // 1000,
					job.job_id
				)
				local hint = input_prompt_hint(out)
				if hint then
					if a.tty == true or a.stdin == true then
						head = head
							.. "\n"
							.. ("[command appears to be waiting for input; send input with job_write(%q) or stop it with job_kill]"):format(
								job.job_id
							)
					else
						head = head .. "\n" .. hint
					end
				end
			else
				head = ("[still running after %dms; moved to background job %s]"):format(limit, job.job_id)
			end
			local report = sandbox_report(h)
			local partial = partial_note(report)
			return {
				content = head .. (out ~= "" and ("\n" .. out) or "") .. (partial and ("\n" .. partial) or ""),
				details = { output = out, job_id = job.job_id, sandbox = report },
			}
		end
		local r = h:read({ max_bytes = cfg.bash_read_bytes })
		local denial, report
		if h.sandbox_denied then
			denial = h:sandbox_denied(r.text or "")
			report = h:sandbox()
		end
		local notes = {}
		if job.kill_reason then
			notes[#notes + 1] = ("[stopped: %s]"):format(job.kill_reason)
		end
		local out, binary = clean_output(r.text or "")
		return bash_result(a, {
			out = out,
			binary = binary,
			dropped = r.dropped,
			code = st.code,
			notes = notes,
			failed = job.kill_reason ~= nil or (st.code == nil and st.signal ~= nil),
			job_id = job.job_id,
			denial = denial,
			sandbox = report,
			program = prog,
		}, spill)
	end

	---------------------------------------------------------------------------
	-- the shell's working directory: a `cd` carries over to the next bash call. Updated only when
	-- a foreground command exits 0; checked before every call and, when the directory is gone,
	-- reset to the session's starting directory with a note to the model; recorded as
	-- `working-directory/changed` in the session and restored on resume; background jobs keep the
	-- directory they started in; an explicit `workdir` runs there without changing the
	-- carried-over directory.
	---------------------------------------------------------------------------
	local function set_shell_cwd(dir, reason)
		if dir == shell_cwd then
			return
		end
		local from = shell_cwd
		shell_cwd = dir
		local session = D.svc.session
		if session then
			local ok, saved, err = pcall(
				session.add_record,
				session,
				"working-directory/changed",
				{ from = from, to = dir, reason = reason }
			)
			if not ok or not saved then
				local message = "Working directory change was not saved: " .. tostring(ok and err or saved)
				mare.log("warn", message)
				if ui and ui.notify then
					pcall(ui.notify, message, "warning")
				end
			end
		end
		ctx.bus:emit("working-directory/changed", { from = from, to = dir, reason = reason })
	end
	--- cwd() -> the directory the next bash command starts in
	function tools.cwd(_, _)
		return shell_cwd
	end
	--- the carried-over directory, or the starting one (with a note) when it no longer exists
	local function ensure_shell_cwd()
		local st = mare.fs.stat(shell_cwd)
		if st and st.kind == "dir" then
			return shell_cwd, nil
		end
		local gone = shell_cwd
		set_shell_cwd(start_cwd, "missing")
		return shell_cwd, ("[working directory %s no longer exists; this command ran in %s]"):format(gone, start_cwd)
	end
	local function session_base_cwd(ev)
		return (ev and type(ev.cwd) == "string" and ev.cwd ~= "") and ev.cwd
			or (D.svc.session and D.svc.session.info and D.svc.session:info().cwd)
			or mare.cwd
	end
	local function on_session_loaded(ev)
		reset_shell()
		local base = session_base_cwd(ev)
		start_cwd = base
		shell_cwd = base
		-- the session plugin decides (resume_cwd): the last recorded shell directory, a chosen
		-- session directory, or nil for the launch directory
		if ev and type(ev.shell_cwd) == "string" then
			shell_cwd = ev.shell_cwd
		end
	end
	local function on_session_new(ev)
		reset_shell()
		local base = session_base_cwd(ev)
		start_cwd = base
		shell_cwd = base
	end
	-- tail reads stop at the newest compaction: write the directory again after one
	local function on_compact_completed(ev)
		if ev and not ev.changed then
			return
		end
		local session = D.svc.session
		if shell_cwd ~= start_cwd and session then
			pcall(session.add_record, session, "working-directory/changed", {
				from = start_cwd,
				to = shell_cwd,
				reason = "compaction",
			})
		end
	end
	if D.lazy_bash_run then
		D.on_session_loaded = on_session_loaded
		D.on_session_new = on_session_new
		D.on_compact_completed = on_compact_completed
	else
		ctx:on("session/loaded", on_session_loaded)
		ctx:on("session/new", on_session_new)
		ctx:on("compact/completed", on_compact_completed)
	end
	local persistent = require("tools.bash_persistent")(D, {
		sandbox_report = sandbox_report,
		ensure_shell_cwd = ensure_shell_cwd,
		set_shell_cwd = set_shell_cwd,
		get_shell_cwd = function()
			return shell_cwd
		end,
		get_detach_gen = function()
			return detach_gen
		end,
	})
	reset_shell = persistent.reset_shell
	ctx:inject({ "session" }, function(sub)
		if sub.session.on_drain then
			sub.session:on_drain(function()
				reset_shell()
			end)
		end
	end)
	local run_persistent = persistent.run_persistent
	local mare_lib = require("mare_lib")
	local shell_lock = mare_lib.mutex()
	local function peek_shell_cwd()
		if mare.fs.stat(shell_cwd) then
			return shell_cwd
		end
		return start_cwd
	end
	local function resolve_workdir(a)
		local base = cfg.shell_session == "persistent" and peek_shell_cwd() or start_cwd
		if a and a.workdir and a.workdir ~= "" then
			local w = a.workdir
			if not string.match(w, "^/") and not string.match(w, "^%a:[/\\]") then
				w = base .. "/" .. w
			end
			return mare.fs.resolve(w)
		end
		return base
	end
	local function with_workdir(a, forced_cwd)
		local a2 = {}
		for k, v in pairs(a) do
			a2[k] = v
		end
		a2._cwd = (type(forced_cwd) == "string" and forced_cwd ~= "") and forced_cwd or resolve_workdir(a)
		return a2
	end
	--- the bash tool: persistent shell, or a temporary one for `workdir`, background jobs,
	--- escalated sandboxes and `shell_session = "fresh"`
	local function bash_with_cwd(a, t, runner)
		local terr = validate_timeout(a)
		if terr then
			return { content = terr, is_error = true }
		end
		local temp = cfg.shell_session ~= "persistent"
			or a.run_in_background
			or a.tty == true
			or a.stdin == true
			or (a.workdir and a.workdir ~= "")
			or a.sandbox_permissions ~= nil
		if temp then
			return runner(with_workdir(a, t and t.cwd), t)
		end
		local r
		do
			local _shell_lease <close> = shell_lock:acquire()
			local expected_cwd = t and t.cwd
			local actual_cwd = peek_shell_cwd()
			if type(expected_cwd) == "string" and expected_cwd ~= "" and actual_cwd ~= expected_cwd then
				return {
					content = ("Tool call blocked: shell working directory changed from %s to %s while waiting for the shell lock"):format(
						expected_cwd,
						actual_cwd
					),
					is_error = true,
					refused = true,
				}
			end
			fg_commands = fg_commands + 1
			local _fg_guard <close> = mare_lib.guard(function()
				fg_commands = fg_commands - 1
			end)
			r = run_persistent(a, t)
		end
		if r.policy_refused then
			-- Per-call elevated sandbox contexts cannot hold a persistent shell; run one-shot.
			return runner(with_workdir(a, t and t.cwd), t)
		end
		-- a flood already streamed to its own spill file (the complete output)
		local spill = r.spill or new_spill()
		local _, nlines = string.gsub(r.out or "", "\n", "")
		if not r.spill and (#(r.out or "") > MAX_BYTES or nlines > MAX_LINES) then
			pcall(mare.proc.spill_write, SPILL, spill, r.out)
		end
		local notes = r.notes or {}
		if r.code then
			notes[#notes + 1] = ("[Command finished with exit code %d]"):format(r.code)
		end
		local out, binary = clean_output(r.out or "")
		local res = bash_result(a, {
			out = out,
			binary = binary,
			dropped = r.dropped,
			code = nil, -- the trailer above reports it
			exit_code = r.code, -- for the MSYS check only
			program = r.program,
			sandbox = r.sandbox,
			denial = r.denial,
			notes = notes,
			failed = r.failed,
			job_id = r.job_id,
		}, spill)
		res.is_error = res.is_error or (r.code ~= nil and r.code ~= 0)
		res.details = res.details or {}
		res.details.code = r.code
		return res
	end

	return {
		bash_direct = bash_direct,
		bash_job = bash_job,
		bash_with_cwd = bash_with_cwd,
		with_workdir = with_workdir,
		resolve_workdir = resolve_workdir,
	}
end
