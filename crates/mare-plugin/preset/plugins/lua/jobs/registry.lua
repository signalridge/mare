-- jobs.registry: the job table, the `jobs` service, exit guard, disposal and resume;
-- part of the `jobs` plugin (plugins/jobs.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local now, owner_of, status_text, elapsed = D.format.now, D.format.owner_of, D.format.status_text, D.format.elapsed
	local notify = D.notify.notify

	local jobs = {} -- id -> job
	local order = {} -- ids in start order

	local function current_session()
		local s = D.svc.session
		if not (s and s.info) then
			return nil, nil
		end
		local ok, info = pcall(s.info, s)
		if not ok or type(info) ~= "table" then
			return nil, nil
		end
		return info.session_id, info.session_epoch
	end

	local function belongs_to_current(job)
		if not job then
			return false
		end
		local sid, sepoch = current_session()
		if job.session_id == nil and sid == nil then
			return true
		end
		return job.session_id == sid and job.session_epoch == sepoch
	end
	D.belongs_to_current = belongs_to_current

	-- exiting with jobs running asks first: the frontend sends a
	-- user's exit through `app/exit-requested` while the guard is on
	local guarded = false
	local function sync_exit_guard()
		local any = false
		for _, j in pairs(jobs) do
			if j.status == "running" and belongs_to_current(j) then
				any = true
				break
			end
		end
		if any ~= guarded and ui.exit_guard then
			guarded = any
			pcall(ui.exit_guard, any)
		end
	end

	local function public_job(job)
		return {
			job_id = job.job_id,
			kind = job.kind,
			description = job.description,
			label = job.label,
			owner = job.owner,
			status = job.status,
			exit_code = job.exit_code,
			exit_signal = job.exit_signal,
			background = job.background,
			started_at_ms = job.started_at_ms and math.floor(job.started_at_ms) or nil,
			ended_at_ms = job.ended_at_ms and math.floor(job.ended_at_ms) or nil,
			kill_requested = job.kill_requested,
			kill_reason = job.kill_reason,
			stop_unconfirmed = job.stop_unconfirmed,
			over_limit = job.over_limit,
		}
	end

	local function changed(job)
		sync_exit_guard()
		-- Local listeners retain the process handle; the public event carries the typed public job shape.
		ctx.bus:emit("jobs/internal-changed", { job = job })
		ctx.bus:emit("jobs/changed", { job = public_job(job) })
	end

	local function record(data)
		if D.svc.session and D.svc.session.add_record then
			local ok, saved, err = pcall(D.svc.session.add_record, D.svc.session, "job", data)
			if not ok or not saved then
				local message = "Background job record was not saved: " .. tostring(ok and err or saved)
				mare.log("warn", message)
				if ui and ui.notify then
					pcall(ui.notify, message, "warning")
				end
			end
		end
	end

	local function running_count(owner)
		local n = 0
		for _, j in pairs(jobs) do
			if j.status == "running" and j.owner == owner and belongs_to_current(j) then
				n = n + 1
			end
		end
		return n
	end

	local function stop_handle(j)
		-- hard stop: TERM → grace → KILL → bounded reap; a tree that can't be confirmed gone is
		-- reported as stop_unconfirmed, never "cleaned up". Custom handles implementing only the
		-- documented `:kill` method fall back to TERM → grace → KILL so completion events stay symmetric.
		ctx:spawn(function()
			if j.handle.stop then
				local ok, r = pcall(j.handle.stop, j.handle, { grace_ms = cfg.kill_grace_ms })
				if ok and r and r.stop_unconfirmed then
					j.stop_unconfirmed = true
					if belongs_to_current(j) then
						changed(j)
					end
				end
			elseif j.handle.kill then
				pcall(j.handle.kill, j.handle, "TERM")
				local ok, st = pcall(j.handle.wait, j.handle, { timeout_ms = cfg.kill_grace_ms })
				if not (ok and st and st.state == "exited") and j.status == "running" then
					pcall(j.handle.kill, j.handle, "KILL")
				end
			end
		end)
	end

	-- ---- lifecycle -----------------------------------------------------------------------

	local function finish(job, st)
		if job.status ~= "running" then
			return
		end
		job.ended_at_ms = math.floor(now())
		job.exit_code = st.code
		job.exit_signal = st.signal
		if job.kill_requested then
			job.status = "cancelled"
		elseif st.code == 0 then
			job.status = "completed"
		else
			job.status = "failed"
		end
		if not belongs_to_current(job) then
			sync_exit_guard()
			return
		end
		record({ event = "end", job_id = job.job_id, status = job.status, exit_code = job.exit_code })
		changed(job)
		notify(job)
	end

	-- Watch job handles and dispatch deferred notifications from host-owned tasks spawned at
	-- plugin apply time so completion, notifications, and wake-up turns never inherit a tool
	-- call's exec_ctx.
	local watch_tx, watch_rx = mare.task.channel()
	ctx:spawn(function()
		while true do
			local job = watch_rx:recv()
			if not job then
				break
			end
			ctx:spawn(function()
				local ok, st = pcall(job.handle.wait, job.handle, {})
				finish(job, ok and st or { code = nil })
			end)
		end
	end)

	local notify_tx, notify_rx = mare.task.channel()
	ctx:spawn(function()
		while true do
			local job = notify_rx:recv()
			if not job then
				break
			end
			notify(job)
		end
	end)

	-- keep at most cfg.max_jobs: drop finished jobs, least recently used first (running jobs are
	-- bounded by max_running and never pruned) unified_exec prune_processes_if_needed
	local function prune()
		local excess = #order - cfg.max_jobs
		if excess <= 0 then
			return
		end
		local finished = {}
		for _, id in ipairs(order) do
			local j = jobs[id]
			-- a stop that couldn't be confirmed stays listed (not reused, still visible)
			if j and j.status ~= "running" and not j.stop_unconfirmed then
				finished[#finished + 1] = j
			end
		end
		table.sort(finished, function(a, b)
			return (a.used or a.ended_at_ms or a.started_at_ms) < (b.used or b.ended_at_ms or b.started_at_ms)
		end)
		local drop = {}
		for i = 1, math.min(excess, #finished) do
			drop[finished[i].job_id] = true
			jobs[finished[i].job_id] = nil
		end
		local kept = {}
		for _, id in ipairs(order) do
			if not drop[id] then
				kept[#kept + 1] = id
			end
		end
		order = kept
	end

	local service = {}

	local function current_session_identity()
		local session = D.svc.session
		if not (session and session.info) then
			return nil, nil
		end
		local ok, info = pcall(session.info, session)
		if ok and type(info) == "table" then
			return info.session_id, info.session_epoch
		end
		return nil, nil
	end

	--- track(spec) -> job: register anything with the handle interface as a job.
	--- spec = {kind, description, handle, owner?, background?}
	function service.track(_, _, spec)
		local kind = spec.kind or "job"
		local sid, sepoch = current_session()
		local launch_sid, launch_epoch = current_session_identity()
		local launch_sb_gen = mare.proc
				and mare.proc.sandbox
				and mare.proc.sandbox.generation
				and mare.proc.sandbox.generation()
			or nil
		local job = {
			job_id = kind .. "-" .. mare.crypto.random_hex(8),
			kind = kind,
			description = spec.description,
			-- short human label for UIs and status reports (jobs/changed {job})
			label = spec.label or spec.description or kind,
			owner = spec.owner or owner_of(nil),
			session_id = sid,
			session_epoch = sepoch,
			handle = spec.handle,
			started_at_ms = math.floor(now()),
			status = "running",
			background = spec.background == true,
			launch_session_id = launch_sid,
			launch_session_epoch = launch_epoch,
			launch_sandbox_gen = launch_sb_gen,
		}
		jobs[job.job_id] = job
		order[#order + 1] = job.job_id
		-- tracking never refuses (a timed-out shell must keep running); only start() enforces
		-- the cap, so flag the overshoot for the caller to tell the model
		if running_count(job.owner) > cfg.max_running then
			job.over_limit = cfg.max_running
		end
		prune()
		record({ event = "start", job_id = job.job_id, kind = kind, description = job.description })
		changed(job)
		watch_tx:send(job)
		return job
	end

	--- start(spec) -> job | nil, err: run a process as a job.
	--- spec = {kind="bash", description, cmd, args, input, cwd, env, spill, owner, background}
	function service.start(self, caller, spec)
		local owner = spec.owner or owner_of(nil)
		if running_count(owner) >= cfg.max_running then
			return nil, ("too many background jobs running (%d); stop one with job_kill first"):format(cfg.max_running)
		end
		local ok, handle = pcall(mare.proc.start, {
			cmd = spec.cmd,
			args = spec.args,
			stdin = spec.stdin,
			input = spec.input,
			tty = spec.tty,
			cols = spec.cols,
			rows = spec.rows,
			cwd = spec.cwd,
			env = spec.env,
			env_include = spec.env_include,
			env_exclude = spec.env_exclude,
			sandbox = spec.sandbox,
			spill = spec.spill,
			spill_prefix = spec.spill_prefix,
			buffer_bytes = cfg.buffer_bytes,
		})
		if not ok then
			return nil, tostring(handle)
		end
		return service.track(self, caller, {
			kind = spec.kind or "bash",
			description = spec.description,
			handle = handle,
			owner = owner,
			background = spec.background,
		})
	end

	--- get(id, owner?) -> job | nil (jobs of other agents or other sessions are invisible)
	function service.get(_, _, id, owner)
		local j = jobs[id]
		if j and belongs_to_current(j) and (owner == nil or j.owner == owner) then
			j.used = now() -- recency for pruning
			return j
		end
		return nil
	end

	local function raw_list(owner)
		local out = {}
		for _, id in ipairs(order) do
			local j = jobs[id]
			if j and belongs_to_current(j) and (owner == nil or j.owner == owner) then
				out[#out + 1] = j
			end
		end
		return out
	end

	--- list(opts?) -> { data = jobs in start order (without internal handle), next_cursor? }
	function service.list(_, _, opts)
		local o = type(opts) == "table" and opts or {}
		local owner = o.owner
		local data = {}
		for _, j in ipairs(raw_list(owner)) do
			data[#data + 1] = public_job(j)
		end
		return require("mare_lib").paginate(data, o)
	end

	--- background(job_id): from now on, finishing sends a notification
	function service.background(_, _, id)
		local j = jobs[id]
		if not j or not belongs_to_current(j) then
			require("mare_lib").fail("jobs.unknown_job", "unknown job: " .. tostring(id))
		end
		j.background = true
		changed(j)
		if j.status ~= "running" and not j.notified then
			notify_tx:send(j)
		end
		return {}
	end

	--- cancel(job_id, reason?, opts?) — SIGTERM the process group, SIGKILL after kill_grace_ms.
	--- Idempotent for already-ended jobs; raises `jobs.unknown_job` if unknown.
	function service.cancel(_, _, id, reason, opts)
		local j = jobs[id]
		if not j or not belongs_to_current(j) then
			require("mare_lib").fail("jobs.unknown_job", "unknown job: " .. tostring(id))
		end
		if j.status ~= "running" then
			return {}
		end
		j.kill_requested = true
		j.kill_reason = reason
		if opts == nil or opts.silent ~= false then
			j.suppress = true
		end
		stop_handle(j)
		changed(j)
		return {}
	end

	local function resolve_read_max(opts)
		local max = cfg.read_max_bytes
		local req_max = opts and tonumber(opts.max_bytes)
		if req_max then
			max = math.max(1, math.min(math.floor(req_max), cfg.read_max_bytes))
		end
		return max
	end

	local function sync_exited(j)
		if j.status == "running" then
			local hst = j.handle:status()
			if hst.state == "exited" then
				finish(j, hst)
			end
		end
	end

	--- Non-consuming byte-offset read for the public `jobs.read` service contract.
	function service.read(_, _, id, opts)
		local j = jobs[id]
		if not j or not belongs_to_current(j) then
			require("mare_lib").fail("jobs.unknown_job", "unknown job: " .. tostring(id))
		end
		local max = resolve_read_max(opts)
		local raw_offset = opts and tonumber(opts.offset)
		local offset = raw_offset and math.max(0, math.floor(raw_offset)) or 0
		local r = j.handle:read_at(offset, { max_bytes = max })
		sync_exited(j)
		local total_bytes = (j.handle:status().bytes or r.next or 0)
		local next_pos = r.next or offset
		local done = j.status ~= "running" and next_pos >= total_bytes
		local parts = {}
		if (r.dropped or 0) > 0 then
			parts[#parts + 1] = ("[%d bytes of earlier output were dropped from the buffer]"):format(r.dropped)
		end
		local text = mare.text.sanitize(r.text or "")
		if text ~= "" then
			parts[#parts + 1] = (string.gsub(text, "\n$", ""))
		end
		if not done and (next_pos < total_bytes or j.status == "running") then
			parts[#parts + 1] = "[more output: call job_output again]"
		end
		parts[#parts + 1] = ("[status: %s]"):format(status_text(j))
		return {
			text = table.concat(parts, "\n"),
			dropped = r.dropped,
			next_offset = (not done) and next_pos or nil,
			done = done,
		}
	end

	--- Consuming read helper used only by model-facing `job_output` / `job_write` tools.
	local function read_unread(id, opts)
		local j = jobs[id]
		if not j or not belongs_to_current(j) then
			require("mare_lib").fail("jobs.unknown_job", "unknown job: " .. tostring(id))
		end
		local max = resolve_read_max(opts)
		local r = j.handle:read({ max_bytes = max })
		if r.done then
			-- the model saw the final state: no completion notice needed
			j.notified = true
		end
		sync_exited(j)
		local parts = {}
		if (r.dropped or 0) > 0 then
			parts[#parts + 1] = ("[%d bytes of earlier output were dropped from the buffer]"):format(r.dropped)
		end
		local text = mare.text.sanitize(r.text or "")
		if text ~= "" then
			parts[#parts + 1] = (string.gsub(text, "\n$", ""))
		end
		if not r.done and j.status ~= "running" then
			parts[#parts + 1] = "[more output: call job_output again]"
		elseif j.status == "running" and (j.handle:status().unread or 0) > 0 then
			parts[#parts + 1] = "[more output: call job_output again]"
		end
		parts[#parts + 1] = ("[status: %s]"):format(status_text(j))
		return {
			text = table.concat(parts, "\n"),
			dropped = r.dropped,
			done = r.done,
		}
	end

	--- write(id, data, opts?) -> ok, err: write to a running job's stdin (or PTY) and optionally
	--- close stdin (`opts.eof = true`).
	function service.write(self, caller, id, data, opts)
		local j = service.get(self, caller, id, owner_of(caller))
		if not j then
			return false, "no such job"
		end
		if j.status ~= "running" then
			return false, ("job %s is not running [status: %s]"):format(j.job_id, status_text(j))
		end
		if j.launch_session_id ~= nil or j.launch_session_epoch ~= nil then
			local cur_sid, cur_epoch = current_session_identity()
			if cur_sid ~= j.launch_session_id or cur_epoch ~= j.launch_session_epoch then
				return false, "cannot write to a job created in another session"
			end
		end
		if
			j.launch_sandbox_gen ~= nil
			and mare.proc
			and mare.proc.sandbox
			and mare.proc.sandbox.generation
			and mare.proc.sandbox.generation() ~= j.launch_sandbox_gen
		then
			return false, "PTY_POLICY_CHANGED: job sandbox policy has changed; stop this job and start a new one"
		end
		j.used = now()
		local want_eof = opts and opts.eof == true
		local has_data = type(data) == "string" and data ~= ""
		if want_eof and not j.handle.close_stdin then
			return false, "cannot close stdin on this job"
		end
		if has_data and not j.handle.write then
			return false, "stdin is closed (job was started without stdin = true or tty = true)"
		end
		if has_data then
			local ok, err = pcall(j.handle.write, j.handle, data, want_eof and { close_stdin = true } or nil)
			if not ok then
				return false, tostring(err)
			end
		end
		if want_eof then
			local ok, err = pcall(j.handle.close_stdin, j.handle)
			if not ok then
				return false, tostring(err)
			end
		end
		return true
	end

	require("mare_lib").service(ctx, "jobs", service)

	-- the process group goes with the plugin (/reload, exit)
	ctx:effect(function()
		return function()
			if guarded then
				pcall(ui.exit_guard, false)
			end
			for _, j in pairs(jobs) do
				if j.status == "running" then
					pcall(j.handle.kill, j.handle, "KILL")
				end
			end
		end
	end, "cleanup")

	-- a user's exit while jobs run: list them, leave only once confirmed. The jobs end with the
	-- process group at exit and stay open in the session, so a resume reports them (below).
	local asking = false
	ctx:on("app/exit-requested", function()
		if asking then
			return
		end
		local running = {}
		for _, id in ipairs(order) do
			local j = jobs[id]
			if j and j.status == "running" then
				running[#running + 1] = j
			end
		end
		if #running == 0 then
			ui.exit({ force = true })
			return
		end
		asking = true
		local _asking_guard <close> = require("mare_lib").guard(function()
			asking = false
		end)
		local lines = {}
		for i, j in ipairs(running) do
			if i > 8 then
				lines[#lines + 1] = ("  … and %d more"):format(#running - 8)
				break
			end
			lines[#lines + 1] = ("  %s  %s (%s)"):format(j.job_id, j.description or j.kind, elapsed(j))
		end
		local ok, yes = pcall(
			ui.confirm,
			("%d background job%s still running — exit and stop %s?\n%s"):format(
				#running,
				#running == 1 and " is" or "s are",
				#running == 1 and "it" or "them",
				table.concat(lines, "\n")
			),
			false
		)
		_asking_guard:release()
		-- a confirmation that can't be shown must never trap the user
		if yes or not ok then
			ui.exit({ force = true })
		end
	end)

	local function drain_jobs(drop_all)
		local kept = {}
		for _, id in ipairs(order) do
			local j = jobs[id]
			if j and not drop_all and belongs_to_current(j) then
				kept[#kept + 1] = id
			elseif j then
				j.suppress = true
				j.notified = true
				if j.status == "running" then
					j.kill_requested = true
					j.kill_reason = "session changed"
					if drop_all then
						if j.handle.stop then
							pcall(j.handle.stop, j.handle, { grace_ms = math.min(cfg.kill_grace_ms or 500, 500) })
						elseif j.handle.kill then
							pcall(j.handle.kill, j.handle, "KILL")
						end
					else
						stop_handle(j)
					end
				end
				jobs[id] = nil
			end
		end
		order = kept
		if D.notify and D.notify.reset then
			D.notify.reset()
		end
		sync_exit_guard()
	end

	-- Terminate and isolate jobs belonging to the outgoing session before a switch publishes,
	-- and also sweep any foreign-session jobs if a session change arrives through another path.
	ctx:inject({ "session" }, function(sub)
		if sub.session.on_drain then
			sub.session:on_drain(function()
				drain_jobs(true)
			end)
		end
	end)
	ctx:on("session/changed", function()
		drain_jobs(false)
	end)

	-- resumed sessions: jobs that were running when the previous process ended are gone
	ctx:on("session/loaded", function()
		if not (D.svc.session and D.svc.session.list_records) then
			return
		end
		local sid, sepoch = current_session()
		local open = {}
		local recs = require("mare_lib").all_records(D.svc.session, "job")
		for _, r in ipairs(recs or {}) do
			if r.event == "start" and r.job_id then
				open[r.job_id] = r
			elseif r.event == "end" and r.job_id then
				open[r.job_id] = nil
			end
		end
		local lost = {}
		for id, r in pairs(open) do
			if not jobs[id] then
				lost[#lost + 1] = ("- %s: %s"):format(id, r.description or r.kind or "job")
				record({ event = "end", job_id = id, status = "interrupted" })
				local ts = math.floor(now())
				jobs[id] = {
					job_id = id,
					kind = r.kind or "job",
					description = r.description,
					label = r.description or r.kind or "job",
					owner = owner_of(nil),
					session_id = sid,
					session_epoch = sepoch,
					status = "interrupted",
					background = false,
					started_at_ms = ts,
					ended_at_ms = ts,
					notified = true,
					handle = {
						status = function()
							return { state = "exited", unread = 0 }
						end,
						read = function()
							return { text = "", dropped = 0, done = true }
						end,
						read_at = function()
							return { text = "", next = 0, dropped = 0 }
						end,
						tail = function()
							return ""
						end,
						kill = function() end,
					},
				}
				order[#order + 1] = id
			end
		end
		-- tell the model with its next request, so it doesn't wait on or read them (
		-- "续接时告诉模型上次退出时这些任务被结束了")
		if #lost > 0 and D.svc.agent then
			table.sort(lost)
			D.svc.agent:notify(
				("[background jobs from the previous run were stopped when mare exited; their processes and any output not yet read are gone — start them again if still needed:\n%s]"):format(
					table.concat(lost, "\n")
				),
				{ wake = false }
			)
		end
	end)

	--- Hold a reader guard on `j` across a wait+read sequence so background completion
	--- notifications (`notify`) do not drain unread output out from under the caller.
	local function with_reader(j, fn)
		j.reading = (j.reading or 0) + 1
		local _guard <close> = setmetatable({}, {
			__close = function()
				j.reading = math.max(0, (j.reading or 1) - 1)
				if j.reading == 0 and j.status ~= "running" and not j.notified then
					notify_tx:send(j)
				end
			end,
		})
		return fn()
	end

	return { service = service, with_reader = with_reader, raw_list = raw_list, read_unread = read_unread }
end
