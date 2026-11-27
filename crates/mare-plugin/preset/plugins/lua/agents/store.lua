-- agents.store: what /agents lists, as plain data (no UI). Three sources:
--   1. this session's sub-session files (`session:children()`, header `parent_session_id` = this
--      session): one member per file, keyed by path; its open/close markers say running,
--      done or crashed (the resume rule; a `running` marker older than this process means it
--      never closed: crashed);
--   2. live events carry the host-assigned subagent identity;
--      only a unique attested identity can associate a live child with a file. Unmatched
--      children remain separate; `workflow/*` supplies run names;
--   3. approvals: the one being asked (`agent/child-approval`, until its `approval/resolved`)
--      and the queue behind it (`agent/approval-queue`), with the host's origin identity:
--      matching live members are marked waiting, with their place in the (FIFO) order.
-- Pure state, read by views; the order is first-seen, never re-sorted.
return function(ctx, svc)
	local M = {}
	-- a `running` marker older than this process (kept across /reload) never closed: crashed
	local state = mare.g["agents"] or {}
	local t0 = state.started_at or mare.task.now()
	state.started_at = t0
	mare.g["agents"] = state
	local heads = {} -- path -> {head = header entry} (read once)
	local tails = {} -- path -> {size, mtime, last = "running"|"closed"|nil, at}
	local live, live_order = {}, {} -- child id -> {id, label, state}
	local asking, asking_request_id, queue = nil, nil, {} -- approvals: the open one, the ones behind it
	local runs = {} -- run id -> {name, total}
	ctx:on("subagent/started", function(e)
		if type(e) == "table" and e.subagent_id and not live[e.subagent_id] then
			local ok, binding = pcall(function()
				return mare.subagent and mare.subagent.identity and mare.subagent.identity(e.subagent_id)
			end)
			local trusted = ok
				and type(binding) == "table"
				and binding.subagent_id == e.subagent_id
				and binding.parent_session_id == e.parent_session_id
				and binding.parent_session_epoch == e.parent_session_epoch
			live[e.subagent_id] = {
				subagent_id = e.subagent_id,
				label = e.label,
				state = "running",
				parent_session_id = e.parent_session_id,
				parent_session_epoch = e.parent_session_epoch,
				attested = trusted,
			}
			live_order[#live_order + 1] = e.subagent_id
		end
	end)
	ctx:on("subagent/completed", function(e)
		local c = type(e) == "table" and live[e.subagent_id]
		if
			c
			and (
				c.parent_session_id == nil
				or (c.parent_session_id == e.parent_session_id and c.parent_session_epoch == e.parent_session_epoch)
			)
		then
			c.state = (e.status == "failed" or e.status == "cancelled" or e.status == "error") and "failed" or "done"
		end
	end)
	ctx:on("agent/approval-queue", function(e)
		queue = type(e) == "table" and type(e.pending) == "table" and e.pending or {}
	end)
	ctx:on("agent/child-approval", function(e)
		asking = type(e) == "table" and e.origin or nil
		asking_request_id = type(e) == "table" and e.request_id or nil
	end)
	ctx:on("pipeline/approval-decided", function(e)
		local o = type(e) == "table" and type(e.call) == "table" and e.call.origin
		local request_id = type(e) == "table" and e.child_approval_id
		if o and asking and asking_request_id ~= nil and request_id == asking_request_id then
			asking, asking_request_id = nil, nil
		end
	end)
	ctx:on("workflow/started", function(e)
		-- Unscoped workflow events cannot safely name a run in the current session.
		local info = svc.session and svc.session:info() or {}
		local cur_epoch = info.session_epoch
		if
			type(e) == "table"
			and e.run_id
			and e.parent_session_id
			and e.parent_session_epoch
			and e.parent_session_id == info.session_id
			and e.parent_session_epoch == cur_epoch
		then
			runs[e.run_id] = {
				name = e.name,
				total = tonumber(e.total),
				parent_session_id = info.session_id,
				parent_session_epoch = cur_epoch,
			}
		end
	end)

	--- the first entry of a sub-session file (read once)
	local function head(path)
		local h = heads[path]
		if not h then
			local ok, entries = pcall(mare.store.read, path, { lenient = true })
			h = { head = ok and type(entries) == "table" and entries[1] or {} }
			heads[path] = h
		end
		return h.head
	end

	--- the file's last open/close marker, re-read only when the file changed
	local function marker(path)
		local st = mare.fs.stat(path)
		local t = tails[path]
		if t and st and t.size == st.size and t.mtime == st.mtime then
			return t.last, t.at, t.activity, t.ok
		end
		local last, at, activity, state_ok
		for _, n in ipairs({ 4, 64 }) do
			local ok, tail = pcall(mare.store.read_tail, path, n)
			for _, e in ipairs(ok and tail or {}) do
				if e.type == "state" then
					last, at, state_ok = e.state, e.created_at_ms, e.ok
				end
				-- what it did last, in one line (commands and tools summarised)
				local m = e.type == "message" and e.message
				if m and m.role == "assistant" then
					for _, b in ipairs(type(m.content) == "table" and m.content or {}) do
						if b.type == "text" and b.text ~= "" then
							activity = b.text:match("[^\n]*[^%s\n][^\n]*") or activity
						elseif b.type == "tool_call" then
							activity = "● " .. tostring(b.name)
						end
					end
				end
			end
			if (last and activity) or not ok then
				break
			end
		end
		tails[path] =
			{ size = st and st.size, mtime = st and st.mtime, last = last, at = at, activity = activity, ok = state_ok }
		return last, at, activity, state_ok
	end

	--- {members, runs, jobs, waiting}: members in first-seen order — runs oldest first, then
	--- each run's members by seq; sub-agents without a file after them
	function M.snapshot()
		local info = svc.session and svc.session:info() or {}
		local sid, epoch = info.session_id, info.session_epoch
		local function belongs(o)
			return o.parent_session_id == nil or (o.parent_session_id == sid and o.parent_session_epoch == epoch)
		end
		local files = {}
		if sid then
			local ok, list = pcall(function()
				return svc.session:children()
			end)
			for _, f in ipairs(ok and list or {}) do
				local h = head(f.path)
				if h.parent_session_id == sid then
					files[#files + 1] = { f = f, h = h, seq = tonumber(f.name:match("^(%d+)%-")) or 0 }
				end
			end
		end
		table.sort(files, function(a, b)
			local ca, cb = a.h.created_at_ms or 0, b.h.created_at_ms or 0
			if a.f.run_id ~= b.f.run_id then
				return ca < cb or (ca == cb and a.f.run_id < b.f.run_id)
			end
			return a.seq < b.seq
		end)
		local members, files_by_id, file_counts = {}, {}, {}
		for _, x in ipairs(files) do
			local meta = type(x.h.meta) == "table" and x.h.meta or {}
			local m = {
				key = "file/" .. x.f.path,
				unassociated = true,
				subagent_id = x.h.session_id,
				run_id = x.f.run_id,
				seq = x.seq,
				label = x.h.label or meta.label or ("agent " .. x.seq),
				type = meta.type or meta.agent,
				phase = meta.phase,
				task = meta.task or meta.description,
				path = x.f.path,
				started = x.h.created_at_ms,
			}
			local last, at, activity, state_ok = marker(x.f.path)
			m.activity = activity
			if last == "closed" then
				m.state = state_ok == false and "failed" or "done"
			elseif last == "running" and (at or 0) < t0 then
				m.state = "crashed"
			else
				m.state = "running"
			end
			members[#members + 1] = m
			if type(m.subagent_id) == "string" then
				files_by_id[m.subagent_id] = m
				file_counts[m.subagent_id] = (file_counts[m.subagent_id] or 0) + 1
			end
		end
		local live_counts = {}
		for _, id in ipairs(live_order) do
			local c = live[id]
			if belongs(c) and c.attested then
				live_counts[c.subagent_id] = (live_counts[c.subagent_id] or 0) + 1
			end
		end
		-- The host supplies the subagent id in the start event. Ambiguous
		-- identities are never joined, even if their labels or run coordinates agree.
		for _, id in ipairs(live_order) do
			local c = live[id]
			if belongs(c) then
				local child_id = c.attested and c.subagent_id or nil
				local m = child_id
						and file_counts[child_id] == 1
						and live_counts[child_id] == 1
						and files_by_id[child_id]
					or nil
				if m then
					m.live, m.attested, m.unassociated = true, true, nil
					if c.state == "failed" then
						m.state = "failed"
					end
				else
					members[#members + 1] = {
						key = "live/" .. tostring(id),
						label = c.label or ("agent-" .. tostring(id)),
						state = c.state,
						subagent_id = id,
						live = true,
						attested = c.attested,
						unassociated = true,
					}
				end
			end
		end
		-- waiting for approval, in the order they will be asked (the open one is #0)
		local order = {}
		local current_asking = asking and belongs(asking)
		if current_asking then
			order[1] = asking
		end
		for _, o in ipairs(queue) do
			if belongs(o) then
				order[#order + 1] = o
			end
		end
		local waiting = 0
		for i, o in ipairs(order) do
			local matched
			for _, m in ipairs(members) do
				if o.subagent_id ~= nil and m.live and m.subagent_id == o.subagent_id and m.waiting == nil then
					m.waiting = current_asking and (i - 1) or i
					waiting = waiting + 1
					matched = true
					break
				end
			end
			if not matched then
				members[#members + 1] = {
					key = "approval/" .. i,
					label = o.label or o.agent_id or "sub-agent",
					state = "running",
					waiting = current_asking and (i - 1) or i,
					unassociated = true,
				}
				waiting = waiting + 1
			end
		end
		local jobs = {}
		if svc.jobs then
			for _, j in ipairs(svc.jobs:list().data) do
				-- sub-agents tracked as jobs are listed as members already
				if not tostring(j.kind or ""):find("agent", 1, true) then
					jobs[#jobs + 1] = j
				end
			end
		end
		local current_runs = {}
		for id, run in pairs(runs) do
			if run.parent_session_id == sid and run.parent_session_epoch == epoch then
				current_runs[id] = run
			end
		end
		return { members = members, runs = current_runs, jobs = jobs, waiting = waiting }
	end

	--- the other mare sessions on this machine, oldest first
	function M.peers()
		local ok, list = pcall(function()
			return mare.peers and mare.peers.list() or {}
		end)
		local me = mare.peers and mare.peers.self()
		local out = {}
		for _, p in ipairs(ok and list or {}) do
			if not (me and me.pid == p.pid) then
				local pl = type(p.plugins) == "table" and p.plugins or {}
				out[#out + 1] = {
					name = tostring(p.name or p.pid),
					pid = p.pid,
					cwd = p.cwd,
					started_at_ms = p.started_at_ms,
					status = type(p.status) == "table" and p.status or {},
					held = type(pl.status) == "table" and tonumber(pl.status.held) or 0,
					session = type(pl.session) == "table" and pl.session or {},
				}
			end
		end
		return out
	end

	return M
end
