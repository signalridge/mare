-- tools.bash_persistent: persistent bash shell lifecycle, marker scanning, and
-- background job promotion for tools.bash_run.
local SHELL_READ = { max_bytes = 1024 * 1024, stream = "stdout" }
local SHELL_RESET = "The persistent bash shell was reset; the next bash call starts with a fresh environment."

local function bash_quote(v)
	return "$'" .. v:gsub("\\", "\\\\"):gsub("'", "\\'"):gsub("\r", "\\r"):gsub("\n", "\\n") .. "'"
end

--- `stop`: false leaves the old shell running (it still has a command to finish), a table
--- is the job's stop options, nil a quick stop
local function reset_shell(s, stop)
	local old = s.pshell
	s.pshell = nil
	if old and stop ~= false then
		s.stopping = s.stopping or {}
		s.stopping[old.h] = true
		local finished = false
		local _stop_guard <close> = require("mare_lib").guard(function()
			s.stopping[old.h] = nil
			if not finished then
				-- Forced coroutine abort during h:stop: issue SIGKILL so stubborn commands do not linger.
				pcall(old.h.kill, old.h, "KILL")
			end
		end)
		pcall(old.h.stop, old.h, type(stop) == "table" and stop or { grace_ms = 500 })
		finished = true
	end
	if stop == nil and s.stopping then
		for h in pairs(s.stopping) do
			s.stopping[h] = nil
			pcall(h.kill, h, "KILL")
		end
	end
end

local function shell_policy_key(s)
	local current = mare.exec.current()
	return s.policy_version() .. "|" .. mare.json.encode(current and current.sandbox or false)
end

local function start_shell(s, cwd, policy_key)
	local sh, err = s.shell_config()
	if not sh then
		return nil, err
	end
	-- the same program, reading its script from stdin (`-lc` → `-l`)
	local args = {}
	for _, a in ipairs(sh.args) do
		local flags = string.match(a, "^%-(%a*)c$")
		if flags then
			if flags ~= "" then
				args[#args + 1] = "-" .. flags
			end
		else
			args[#args + 1] = a
		end
	end
	local pol = s.tools.env_policy(s.tools, s.ctx)
	local ok, h = pcall(mare.proc.start, {
		cmd = sh.path,
		args = args,
		cwd = cwd,
		stdin = true,
		on_policy_change = "stop",
		env = s.tools.process_env(s.tools, s.ctx),
		env_include = pol.env_include,
		env_exclude = pol.env_exclude,
		buffer_bytes = 8 * 1024 * 1024,
	})
	if not ok then
		local msg = tostring(h)
		return nil, msg, string.find(msg, "PTY_POLICY", 1, true) ~= nil
	end
	-- the prelude (and the scrub of what it activates) runs once, at the start
	local _, pargs, input = s.tools.shell_command(s.tools, s.ctx, ":")
	local script = input or (pargs and pargs[#pargs]) or ":"
	pcall(h.write, h, script .. "\n")
	s.pshell = { h = h, policy = policy_key, program = sh.path }
	return s.pshell
end

local function build_command_line(s, command, START, END, pwd)
	-- stdin is empty: a command that reads it gets EOF instead of eating the
	-- shell's control channel or hanging
	-- the session's MARE_* values are re-exported before every command: the shell outlives
	-- /model, /effort and session switches, and children must see this session's current
	-- values
	local exports = {}
	local penv = s.tools.process_env(s.tools, nil)
	for _, k in ipairs({
		"MARE_SESSION_ID",
		"MARE_SESSION_MODEL",
		"MARE_SESSION_EFFORT",
		"MARE_CWD",
		"MARE_PEER_SOCKET",
	}) do
		if penv[k] ~= nil then
			exports[#exports + 1] = ("export %s=%s; "):format(k, bash_quote(tostring(penv[k])))
		else
			exports[#exports + 1] = ("unset %s; "):format(k)
		end
	end
	-- stderr joins stdout for the command (2>&1): on two pipes, stderr written before the
	-- START marker is read would be dropped as pre-command noise, and its order against
	-- stdout would be lost
	-- exports are prepended after formatting: their values may contain `%`
	return table.concat(exports)
		.. ("printf '%%s\\n' %s; eval -- %s </dev/null 2>&1; __mare_s=$?; printf '%%s%%s:%%s\\n' %s \"$__mare_s\" \"%s\"\n"):format(
			bash_quote(START),
			bash_quote(command),
			bash_quote(END),
			pwd
		)
end

-- output is scanned incrementally: each read is searched for the next marker together
-- with a marker-length carry, never the whole output again; the body is kept bounded
-- stdout only: the command's own stderr joins it (2>&1 above), while the shell's stderr
-- carries its xtrace / verbose echo of this wrapper line (`set -x` / `set -v` left on
-- by an earlier command) — markers included, which leaked into the output and hid the
-- real END marker
local function new_scanner(s, h, START, END)
	local acc = s.new_output_acc(s.cfg.shell_buffer_bytes)
	local sc = {
		acc = acc,
		phase = "start",
		carry = "",
		trailer = "",
		sink = function(chunk)
			acc:push(chunk)
		end,
	}
	function sc.feed(text)
		if sc.phase == "start" then
			local win = sc.carry .. text
			local sp = string.find(win, START, 1, true)
			if not sp then
				sc.carry = string.sub(win, -(#START - 1))
				return
			end
			sc.phase, sc.carry, text = "body", "", string.sub(win, sp + #START)
		end
		if sc.phase == "body" then
			local win = sc.carry .. text
			local e = string.find(win, END, 1, true)
			if not e then
				local keep = math.min(#win, #END - 1)
				sc.sink(string.sub(win, 1, #win - keep))
				sc.carry = string.sub(win, #win - keep + 1)
				return
			end
			sc.sink(string.sub(win, 1, e - 1))
			sc.phase, sc.carry, text = "trailer", "", string.sub(win, e + #END)
		end
		sc.trailer = sc.trailer .. text
	end
	--- what was produced so far (on exit, cancellation or timeout): the body plus any
	--- unscanned carry. `keep_carry`: the scan goes on (moved to the background), so a
	--- carry that may hold the start of the END marker stays for it.
	function sc.so_far(keep_carry)
		if sc.phase == "body" then
			-- keep only the part of the carry that could still begin the END marker
			local keep = 0
			if keep_carry then
				for k = math.min(#sc.carry, #END - 1), 1, -1 do
					if string.sub(sc.carry, -k) == string.sub(END, 1, k) then
						keep = k
						break
					end
				end
			end
			acc:push(string.sub(sc.carry, 1, #sc.carry - keep))
			sc.carry = string.sub(sc.carry, #sc.carry - keep + 1)
		end
		local res = acc:result()
		res.out = string.gsub(res.out, "^\r?\n", "")
		res.sandbox = s.sandbox_report(h)
		return res
	end
	return sc
end

--- After the move to the background: a job handle for this one command, not for the
--- shell. It
--- keeps scanning the shell's output for this command's END marker and the job ends
--- there; the shell isn't reused (the next call already gets a fresh one), so it is
--- stopped then. Same interface as a mare.proc job (see the jobs plugin).
local function new_command_handle(s, h, sc)
	local cap = s.cfg.shell_buffer_bytes
	local q = { unread = "", lost = 0, tail = "", total = 0, started = mare.task.clock() }
	sc.sink = function(chunk)
		if chunk == "" then
			return
		end
		q.total = q.total + #chunk
		q.unread = q.unread .. chunk
		if #q.unread > cap then
			local start = s.utf8_align_start(q.unread, #q.unread - cap + 1)
			q.lost = q.lost + (start - 1)
			q.unread = string.sub(q.unread, start)
		end
		q.tail = q.tail .. chunk
		if #q.tail > cap then
			local start = s.utf8_align_start(q.tail, #q.tail - cap + 1)
			q.tail = string.sub(q.tail, start)
		end
	end
	local function ended(code, signal)
		q.state, q.code, q.signal, q.ended = "exited", code, signal, mare.task.clock()
	end
	local function trailer_code()
		return sc.phase == "trailer" and tonumber(string.match(sc.trailer, "^(%d+):[^\n]*\n")) or nil
	end
	local function pump()
		if q.state then
			return
		end
		local r = h:read(SHELL_READ)
		if r.text and r.text ~= "" then
			sc.feed(r.text)
		end
		local code = trailer_code()
		if code then
			ended(code)
			s.ctx:spawn(function()
				pcall(h.stop, h, { grace_ms = 500 })
			end)
			return
		end
		local st = h:status()
		if st.state == "exited" then
			sc.feed((h:read(SHELL_READ)).text or "")
			if sc.phase == "body" then
				sc.sink(sc.carry)
				sc.carry = ""
			end
			ended(trailer_code() or st.code, st.signal)
		end
	end
	local hd = {}
	function hd.status()
		pump()
		return {
			state = q.state or "running",
			code = q.code,
			signal = q.signal,
			elapsed_ms = (q.ended or mare.task.clock()) - q.started,
			bytes = q.total,
			unread = #q.unread + q.lost,
		}
	end
	function hd.read(_, o)
		pump()
		local max = (o and o.max_bytes) or 50 * 1024
		local stop = #q.unread > max and s.utf8_align_end(q.unread, max) or #q.unread
		local text = string.sub(q.unread, 1, stop)
		q.unread = string.sub(q.unread, stop + 1)
		local dropped = q.lost
		q.lost = 0
		return { text = text, dropped = dropped, done = q.state ~= nil and q.unread == "" }
	end
	function hd.read_at(_, offset, o)
		pump()
		local req = math.max(0, math.floor(offset or 0))
		local base = q.total - #q.tail
		local dropped = 0
		if req < base then
			dropped = base - req
			req = base
		end
		local max = (o and o.max_bytes) or 50 * 1024
		local rel = req - base + 1
		local slice = ""
		if rel <= #q.tail then
			local avail = string.sub(q.tail, rel)
			local stop = #avail > max and s.utf8_align_end(avail, max) or #avail
			slice = string.sub(avail, 1, stop)
		end
		return { text = slice, next = math.min(q.total, req + #slice), dropped = dropped }
	end
	function hd.tail(_, n)
		pump()
		local want = n or 4096
		if #q.tail <= want then
			return q.tail
		end
		return string.sub(q.tail, s.utf8_align_start(q.tail, #q.tail - want + 1))
	end
	function hd.wait(self, o)
		local deadline = o and o.timeout_ms and (mare.task.clock() + o.timeout_ms)
		local cancel = o and o.cancel
		while true do
			pump()
			if q.state then
				return self:status()
			end
			if cancel and cancel:is_cancelled() then
				local st = self:status()
				st.cancelled = true
				return st
			end
			local left = deadline and (deadline - mare.task.clock()) or 100
			if left <= 0 then
				local st = self:status()
				st.timed_out = true
				return st
			end
			h:wait({ timeout_ms = math.min(100, left), cancel = cancel })
		end
	end
	function hd.kill(_, sig)
		return h:kill(sig)
	end
	function hd.stop(_, opts_stop)
		return h:stop(opts_stop)
	end
	return hd
end

local function classify_denial(h, out)
	if not h.sandbox_denied then
		return nil
	end
	local ok, d = pcall(h.sandbox_denied, h, out)
	if ok and d and (not d.host or string.find(out, d.host, 1, true)) then
		return d
	end
	return nil
end

local function finish_trailer(s, code, dir, h, sc, notes, fresh)
	local res = sc.so_far()
	res.out = string.gsub(res.out, "\r?\n$", "")
	if dir and dir ~= "" then
		if mare.os == "windows" then
			dir = string.gsub(dir, "/", "\\")
		end
		if dir ~= s.get_shell_cwd() then
			s.set_shell_cwd(dir, "cd")
			notes[#notes + 1] = ("[working directory is now %s]"):format(dir)
		end
	end
	res.code, res.notes, res.fresh = tonumber(code), notes, fresh
	-- a failure the sandbox caused, with the path or host it refused (the
	-- permissions plugin's hint names it), like the one-shot runs. The shell's
	-- proxy outlives commands: a refused host counts only when this output names it
	if res.code ~= 0 then
		res.denial = classify_denial(h, res.out)
	end
	return res
end

local function finish_shell_exit(s, st, h, sh_state, sc, notes)
	reset_shell(s, false)
	local rest = h:read(SHELL_READ)
	sc.feed(rest.text or "")
	if sc.phase == "start" then
		-- the marker never came: show what there was
		sc.acc:push(sc.carry)
		sc.carry = ""
	end
	local res = sc.so_far()
	-- what bash_result needs to recognise a shell that couldn't start (MSYS under
	-- the Windows sandbox: SANDBOX_MSYS_UNSUPPORTED)
	res.program = sh_state.program
	res.sandbox = h.sandbox and select(2, pcall(h.sandbox, h)) or nil
	notes[#notes + 1] = st.signal and ("[shell killed by signal: %s]"):format(st.signal)
		or ("[shell exited: code %s]"):format(tostring(st.code))
	notes[#notes + 1] = SHELL_RESET
	res.code, res.notes, res.failed = st.code, notes, true
	if st.code ~= 0 then
		res.denial = classify_denial(h, res.out)
	end
	return res
end

local function finish_background_left(s, a, st, h, sh_state, sc, notes)
	reset_shell(s, false)
	local rest = h:read(SHELL_READ)
	if rest.text and rest.text ~= "" then
		sc.feed(rest.text)
	end
	if sc.phase == "start" then
		sc.acc:push(sc.carry)
		sc.carry = ""
	end
	local J = s.svc.jobs
	local id
	local job_over_limit
	local res
	if J then
		res = sc.so_far(true)
		local job = J:track({
			kind = "bash",
			description = mare.text.trunc_width((string.gsub(a.command, "%s+", " ")), 60),
			handle = new_command_handle(s, h, sc),
			background = true,
		})
		id = job and job.job_id
		job_over_limit = job and job.over_limit
	else
		pcall(h.stop, h, { grace_ms = 500 })
		res = sc.so_far()
	end
	res.program = sh_state.program
	res.sandbox = h.sandbox and select(2, pcall(h.sandbox, h)) or nil
	notes[#notes + 1] = id
			and ("[the command exited; background processes it started are still running as background job %s — check with job_output or stop with job_kill]"):format(
				id
			)
		or "[the command exited; background processes it started were stopped]"
	if id and job_over_limit then
		notes[#notes + 1] = ("[this job exceeds the limit of %d running background jobs; stop one with job_kill]"):format(
			job_over_limit
		)
	end
	notes[#notes + 1] = SHELL_RESET
	res.code, res.notes, res.failed, res.job_id = st.code, notes, id == nil, id
	return res
end

local function promote_to_background(s, a, h, sc, notes, limit, now, t0, stalled, detached)
	-- outlived its timeout, silent for `stall_ms`, or sent away by the user (Ctrl+B):
	-- this command keeps running as a background job that ends
	-- with the command, and the next call gets a fresh shell
	reset_shell(s, false)
	local J = s.svc.jobs
	local id
	local job_over_limit
	local res
	if J then
		res = sc.so_far(true)
		local job = J:track({
			kind = "bash",
			description = mare.text.trunc_width((string.gsub(a.command, "%s+", " ")), 60),
			handle = new_command_handle(s, h, sc),
			background = true,
		})
		id = job and job.job_id
		job_over_limit = job and job.over_limit
	else
		pcall(h.stop, h, { grace_ms = 500 })
	end
	if detached and id then
		notes[#notes + 1] = ("[moved to background job %s by the user — check it with job_output]"):format(id)
	elseif stalled and now - t0 < limit then
		notes[#notes + 1] = id
				and ("[no output for %ds; still running as background job %s — check it with job_output, or stop it with job_kill]"):format(
					s.cfg.stall_ms // 1000,
					id
				)
			or "[command produced no output and was stopped]"
	else
		notes[#notes + 1] = id and ("[still running after %dms; moved to background job %s]"):format(limit, id)
			or ("[command timed out after %.1fs]"):format((now - t0) / 1000)
	end
	if id and job_over_limit then
		notes[#notes + 1] = ("[this job exceeds the limit of %d running background jobs; stop one with job_kill]"):format(
			job_over_limit
		)
	end
	res = res or sc.so_far()
	local hint = s.input_prompt_hint(res.out)
	if hint then
		notes[#notes + 1] = hint
	end
	notes[#notes + 1] = SHELL_RESET
	res.notes, res.failed, res.job_id = notes, id == nil, id
	return res
end

--- run one command in the persistent shell: {out, code, notes, failed, job_id?}
local function run_persistent(s, a, t)
	local base, note = s.ensure_shell_cwd()
	local policy_key = shell_policy_key(s)
	if s.pshell and s.pshell.policy ~= policy_key then
		reset_shell(s)
	end
	local notes = {}
	if note then
		notes[#notes + 1] = note
		reset_shell(s)
	end
	local fresh = s.pshell == nil
	local sh_state, err, policy_refused = s.pshell, nil, false
	if not sh_state then
		sh_state, err, policy_refused = start_shell(s, base, policy_key)
	end
	if not sh_state then
		return {
			out = "Could not start the shell: " .. tostring(err),
			failed = true,
			policy_refused = policy_refused or nil,
			notes = notes,
		}
	end
	local h = sh_state.h
	local nonce = mare.crypto.random_hex(8)
	local START, END = "__MARE_BASH_START_" .. nonce .. "__", "__MARE_BASH_END_" .. nonce .. ":"
	local pwd = mare.os == "windows" and "$( (pwd -W 2>/dev/null || pwd -P) )" or "$(pwd -P 2>/dev/null)"
	local line = build_command_line(s, a.command, START, END, pwd)
	if not pcall(h.write, h, line) then
		reset_shell(s)
		return {
			out = "",
			failed = true,
			notes = { "[shell exited before the command could be sent]", SHELL_RESET },
		}
	end
	local settled = false
	local _run_guard <close> = require("mare_lib").guard(function()
		if not settled and s.pshell and s.pshell.h == h then
			s.pshell = nil
			pcall(h.kill, h, "KILL")
		end
	end)
	local sc = new_scanner(s, h, START, END)
	local last = 0
	local limit = math.min(a.timeout_ms or s.cfg.bash_timeout_ms, s.cfg.bash_max_timeout_ms)
	local t0 = mare.task.clock()
	local last_out = t0
	local gen0 = s.get_detach_gen()
	while true do
		local r = h:read(SHELL_READ)
		if r.text and r.text ~= "" then
			sc.feed(r.text)
			last_out = mare.task.clock()
		end
		if sc.phase == "trailer" then
			local code, dir = string.match(sc.trailer, "^(%d+):([^\n]*)\n")
			if code then
				settled = true
				return finish_trailer(s, code, dir, h, sc, notes, fresh)
			end
		end
		if t.update and mare.task.clock() - last > s.cfg.update_interval_ms then
			last = mare.task.clock()
			t.update({ output = sc.acc:tail_of(s.cfg.live_tail_bytes) })
		end
		local st = h:wait({ timeout_ms = 50, cancel = t.cancel })
		if st.state == "exited" then
			settled = true
			return finish_shell_exit(s, st, h, sh_state, sc, notes)
		end
		if st.background_left then
			settled = true
			return finish_background_left(s, a, st, h, sh_state, sc, notes)
		end
		if st.cancelled or (t.cancel and t.cancel:is_cancelled()) then
			-- the user's interrupt: SIGINT, 1 s, SIGTERM, the grace
			-- period, SIGKILL — a second Ctrl+C forces it. The signals go to the shell's
			-- process group, which the command shares (no job control); the shell is
			-- reset either way
			reset_shell(s, { interrupt = true })
			settled = true
			notes[#notes + 1] = "[command aborted]"
			notes[#notes + 1] = SHELL_RESET
			local res = sc.so_far()
			res.notes, res.failed = notes, true
			return res
		end
		local now = mare.task.clock()
		local stalled = s.cfg.stall_ms > 0 and now - last_out >= s.cfg.stall_ms
		local detached = s.get_detach_gen() ~= gen0
		if now - t0 >= limit or stalled or detached then
			settled = true
			return promote_to_background(s, a, h, sc, notes, limit, now, t0, stalled, detached)
		end
	end
end

return function(D, opts)
	-- One persistent shell: commands are sent
	-- on stdin wrapped in nonce markers — `printf START; eval -- $'cmd'; s=$?; printf END s:pwd`
	-- — and run in order. exit, timeout, cancellation or a write failure: reset, never repair; the
	-- next call starts fresh (in the last directory that still exists).
	-- pshell = { h = job handle, policy = policy version it was started under }
	local s = {
		ctx = D.ctx,
		cfg = D.cfg,
		svc = D.svc,
		tools = D.registry.tools,
		policy_version = D.approval.policy_version,
		shell_config = D.shell_env.shell_config,
		input_prompt_hint = D.bash_output.input_prompt_hint,
		new_output_acc = D.bash_output.new_output_acc,
		utf8_align_start = D.bash_output.utf8_align_start,
		utf8_align_end = D.bash_output.utf8_align_end,
		sandbox_report = opts.sandbox_report,
		ensure_shell_cwd = opts.ensure_shell_cwd,
		set_shell_cwd = opts.set_shell_cwd,
		get_shell_cwd = opts.get_shell_cwd,
		get_detach_gen = opts.get_detach_gen,
		pshell = nil,
	}

	-- the shell ends with the plugin (reload, shutdown): `ctx:on("dispose")` was an event
	-- nobody emits, so the shell outlived both — on Windows its open output pipes kept a
	-- blocking reader, and mare, from exiting
	s.ctx:effect(function()
		return function()
			reset_shell(s)
		end
	end, "persistent shell")

	return {
		reset_shell = function(stop)
			return reset_shell(s, stop)
		end,
		run_persistent = function(a, t)
			return run_persistent(s, a, t)
		end,
	}
end
