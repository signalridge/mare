-- jobs.tools: the job_output / job_list / job_kill tools; part of the `jobs` plugin (plugins/jobs.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local owner_of, status_text = D.format.owner_of, D.format.status_text
	local service = D.registry.service
	local with_reader = D.registry.with_reader

	-- ---- tools ---------------------------------------------------------------------------

	local function lookup(id, tctx)
		local j = service.get(service, ctx, id, owner_of(tctx))
		if not j then
			return nil,
				{ content = ("No such job: %s (job_list shows your jobs)"):format(tostring(id)), is_error = true }
		end
		return j
	end

	--- the last `n` lines of the job's output (at most max_bytes), without consuming anything
	local function tail_lines(j, n, max)
		local raw = mare.text.sanitize(j.handle:tail(max) or "")
		raw = string.gsub(raw, "\n$", "")
		local lines = {}
		for line in string.gmatch(raw .. "\n", "([^\n]*)\n") do
			lines[#lines + 1] = line
		end
		local from = math.max(1, #lines - n + 1)
		return table.concat(lines, "\n", from)
	end

	ctx.tools:register("job_output", {
		snippet = "Read new output of a background job",
		description = "Return the output a background job produced since the last call, ending with a "
			.. "[status: ...] line. Non-blocking by default; wait = true blocks until the job ends, "
			.. ("timeout_ms passes (default %d, max %d) or the user sends a message — the job keeps running. "):format(
				cfg.wait_timeout_ms,
				cfg.max_wait_ms
			)
			.. "until = <regex> also waits, but returns as soon as new output matches it (e.g. a server's "
			.. "'listening on'). tail = N returns the last N lines without consuming anything (a peek; "
			.. "the next call still returns everything new). max_bytes limits how much one call returns.",
		parameters = {
			type = "object",
			properties = {
				job_id = { type = "string", description = "Job id, e.g. bash-3f9c0a1b2c3d4e5f" },
				wait = { type = "boolean", description = "Block until the job finishes (default false)" },
				timeout_ms = { type = "integer", description = "Longest wait in milliseconds" },
				["until"] = {
					type = "string",
					description = "Regex: wait until new output matches it (or the job ends, or timeout_ms passes)",
				},
				tail = {
					type = "integer",
					description = "Return only the last N lines, without consuming the output",
				},
				max_bytes = {
					type = "integer",
					description = ("Return at most this many bytes (default and max %d)"):format(cfg.read_max_bytes),
				},
			},
			required = { "job_id" },
		},
		side_effect = "read",
		timeout_ms = cfg.max_wait_ms + 60000,
		summary = function(a)
			local extra = (a["until"] and (", until " .. a["until"])) or (a.tail and (", tail " .. a.tail))
			return ("JobOutput(%s%s)"):format(a.job_id or "?", extra or (a.wait and ", wait" or ""))
		end,
		execute = function(a, t)
			local j, err = lookup(a.job_id, t)
			if not j then
				return err
			end
			local max = cfg.read_max_bytes
			local req_max = tonumber(a.max_bytes)
			if req_max then
				max = math.max(1, math.min(math.floor(req_max), cfg.read_max_bytes))
			end
			local req_tail = tonumber(a.tail)
			if req_tail then
				local n = math.max(1, math.floor(req_tail))
				local text = tail_lines(j, n, max)
				return {
					content = (text ~= "" and (text .. "\n") or "")
						.. ("[tail: last %d lines, nothing consumed]\n[status: %s]"):format(n, status_text(j)),
				}
			end
			local re
			local pat = a["until"]
			if type(pat) == "string" and pat ~= "" then
				local rerr
				re, rerr = mare.text.regex(pat)
				if not re then
					return { content = "Invalid until regex: " .. tostring(rerr), is_error = true }
				end
			end
			return with_reader(j, function()
				local matched = false
				local function match_unread()
					local un = j.handle:status().unread or 0
					return un > 0 and re:is_match(j.handle:tail(math.min(un, cfg.buffer_bytes)) or "")
				end
				if (a.wait or re) and j.status == "running" then
					local ms = math.max(0, math.min(a.timeout_ms or cfg.wait_timeout_ms, cfg.max_wait_ms))
					-- the user interjecting ends the wait early (the job keeps running)
					local tok = t.cancel and t.cancel:child() or mare.task.token()
					local off = ctx:on("queue/changed", function(ev)
						if ev and ev.action == "enqueued" then
							tok:cancel()
						end
					end)
					if re then
						-- poll the unread output (a non-consuming peek) until it matches
						local deadline = mare.task.clock() + ms
						while not tok:is_cancelled() do
							if match_unread() then
								matched = true
								break
							end
							local left = deadline - mare.task.clock()
							if left <= 0 then
								break
							end
							local ok, st =
								pcall(j.handle.wait, j.handle, { timeout_ms = math.min(200, left), cancel = tok })
							if ok and st and st.state == "exited" then
								break
							end
						end
					else
						pcall(j.handle.wait, j.handle, { timeout_ms = ms, cancel = tok })
					end
					off()
				end
				if re and not matched then
					matched = match_unread()
				end
				local text = D.registry.read_unread(j.job_id, { max_bytes = max }).text
				if re then
					text = (matched and "[until: matched]\n" or "[until: no match yet]\n") .. text
				end
				return { content = text }
			end)
		end,
	})

	ctx.tools:register("job_list", {
		snippet = "List background jobs",
		description = "List your background jobs, one per line: <id> [<kind>] <status> — <description>.",
		parameters = { type = "object", properties = {} },
		side_effect = "read",
		summary = function()
			return "JobList"
		end,
		execute = function(_, t)
			local lines = {}
			for _, j in ipairs(D.registry.raw_list(owner_of(t))) do
				lines[#lines + 1] = ("%s [%s] %s — %s"):format(j.job_id, j.kind, status_text(j), j.description or "")
			end
			return { content = #lines > 0 and table.concat(lines, "\n") or "No background jobs." }
		end,
	})

	ctx.tools:register("job_write", {
		side_effect = "external",
		idempotent = false,
		snippet = "Write to a running job's stdin",
		description = "Write bytes to a running background job's stdin (or PTY) and return new output, "
			.. "ending with a [status: ...] line. Omit input or pass an empty string to poll without writing. "
			.. "The job must have been started with stdin = true or tty = true.",
		parameters = {
			type = "object",
			properties = {
				job_id = { type = "string", description = "Job id, e.g. bash-3f9c0a1b2c3d4e5f" },
				input = {
					type = "string",
					description = "Bytes to write to stdin (include \\n for Enter, or control chars like \\u0003 for Ctrl+C). Omit or empty to poll.",
				},
				eof = {
					type = "boolean",
					description = "Close stdin after writing (pipe jobs only; sends EOF)",
				},
				timeout_ms = {
					type = "integer",
					description = "Milliseconds to wait for output after writing (default 250 when writing, 1000 when polling; max 30000)",
				},
				max_bytes = {
					type = "integer",
					description = ("Return at most this many bytes (default and max %d)"):format(cfg.read_max_bytes),
				},
			},
			required = { "job_id" },
		},
		timeout_ms = 60000,
		summary = function(a)
			return ("JobWrite(%s)"):format(a.job_id or "?")
		end,
		execute = function(a, t)
			local j, err = lookup(a.job_id, t)
			if not j then
				return err
			end
			local data = type(a.input) == "string" and a.input or ""
			local want_eof = a.eof == true
			local max = cfg.read_max_bytes
			local max_n = tonumber(a.max_bytes)
			if max_n then
				max = math.max(1, math.min(math.floor(max_n), cfg.read_max_bytes))
			end
			if j.status ~= "running" then
				if data ~= "" or want_eof then
					return {
						content = ("Job %s is not running [status: %s]"):format(j.job_id, status_text(j)),
						is_error = true,
					}
				end
				return { content = D.registry.read_unread(j.job_id, { max_bytes = max }).text }
			end
			return with_reader(j, function()
				if data ~= "" or want_eof then
					local ok, werr = service.write(service, t, j.job_id, data, { eof = want_eof })
					if not ok then
						return {
							content = ("Failed to write to job %s: %s"):format(j.job_id, tostring(werr)),
							is_error = true,
						}
					end
				end
				local default_wait = (data ~= "" or want_eof) and 250 or 1000
				local wait_n = tonumber(a.timeout_ms)
				local ms = math.max(0, math.min(wait_n or default_wait, 30000))
				if ms > 0 and j.status == "running" then
					local tok = t.cancel and t.cancel:child() or mare.task.token()
					local off = ctx:on("queue/changed", function(ev)
						if ev and ev.action == "enqueued" then
							tok:cancel()
						end
					end)
					pcall(j.handle.wait, j.handle, { timeout_ms = ms, cancel = tok })
					off()
					if t.cancel and t.cancel:is_cancelled() then
						local text = D.registry.read_unread(j.job_id, { max_bytes = max }).text
						return { content = "[cancelled]\n" .. text, is_error = true }
					end
				end
				return { content = D.registry.read_unread(j.job_id, { max_bytes = max }).text }
			end)
		end,
	})

	ctx.tools:register("job_kill", {
		side_effect = "external",
		idempotent = true,
		snippet = "Stop a background job",
		description = "Request a background job to stop (its process group gets SIGTERM, then SIGKILL). "
			.. "The status becomes killed once the process has really stopped.",
		parameters = {
			type = "object",
			properties = {
				job_id = { type = "string", description = "Job id" },
				reason = { type = "string", description = "Why it is being stopped" },
			},
			required = { "job_id" },
		},
		summary = function(a)
			return ("JobKill(%s)"):format(a.job_id or "?")
		end,
		execute = function(a, t)
			local j, err = lookup(a.job_id, t)
			if not j then
				return err
			end
			if j.status ~= "running" then
				return { content = ("Job %s is not running [status: %s]"):format(j.job_id, status_text(j)) }
			end
			service.cancel(service, ctx, j.job_id, a.reason or "requested by the model")
			return { content = ("Stop requested for %s."):format(j.job_id) }
		end,
	})
end
