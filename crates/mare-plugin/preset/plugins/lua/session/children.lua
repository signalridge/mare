-- session.children: sub-session files (session:child / children) and their live
-- transcript view;
-- part of the `session` plugin (plugins/session.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg, S, session = D.ctx, D.cfg, D.S, D.session

	-- The host calls this before starting an automatically recorded child. The parent
	-- header must exist so its auxiliary files remain discoverable and deletable.
	function session.prepare_children(_, _)
		if D.disabled or S.readonly then
			return nil, "child transcripts require a writable saved session"
		end
		local owner = { id = S.id, generation = S.generation }
		return D.store.run_host(function(valid)
			D.store.ensure_file(valid)
			return true
		end, owner)
	end

	--- child(spec) -> handle: a sub-session
	--- stored as `<session dir>/agents/<run_id>/<seq>-<label>.jsonl` in the normal session
	--- format, written and locked through the same store. spec = {run_id, seq, label, meta?}.
	--- handle: {path, id, append(entry, opts), close()} — `append` takes ordinary entries
	--- (e.g. {type="message", message=...}); ids/parents are filled in. The file carries the
	--- session's open/close markers ({type="state"} "running" now, "closed" on close), so a
	--- reader (the agents list) tells a running, a finished and a crashed sub-agent apart by the
	--- same rule as a resumed session.
	function session.child(_, caller, spec)
		if D.disabled then
			error("sub-session transcripts are unavailable without a saved session", 0)
		end
		assert(type(spec) == "table" and spec.run_id, "session:child needs run_id")
		local run_id = tostring(spec.run_id)
		assert(run_id:match("^[%w_-]+$"), "session:child run_id must be a single safe name")
		local seq = tonumber(spec.seq) or 0
		assert(seq >= 0 and seq <= 999999 and seq % 1 == 0, "session:child seq must be a nonnegative integer")
		local label = tostring(spec.label or "agent"):gsub("[^%w%-_.]", "-")
		local original_label = spec.label ~= nil and tostring(spec.label) or nil
		local meta = D.store.copy_from_tool(spec.meta)
		local owner = { id = S.id, generation = S.generation }
		local dir = D.child_dir()
		local function close_abandoned(h)
			h.close()
		end
		local handle, err = D.store.run_host(function(valid)
			-- Keep the parent writer lock through creation. Parent deletion holds the same
			-- lock while enumerating and locking every child.
			D.store.ensure_file(valid)
			if valid and not valid() then
				return nil, "session writer request was cancelled or its owner changed"
			end
			local path = ("%s/agents/%s/%03d-%s.jsonl"):format(dir, run_id, seq, label)
			local file = mare.store.open(path, { auxiliary = true })
			local id = mare.crypto.uuid()
			local _, header_lossy = file:append({
				type = "session",
				version = mare.store.format_version,
				session_id = id,
				cwd = mare.cwd,
				created_at_ms = require("mare_lib").to_epoch_ms(mare.task.now()),
				parent_session_id = owner.id,
				run_id = run_id,
				label = original_label,
				meta = meta,
			}, { sync = true })
			D.store.warn_lossy_utf8(header_lossy)
			local lib = require("mare_lib")
			file:append(
				{ type = "state", state = "running", created_at_ms = lib.to_epoch_ms(mare.task.now()) },
				{ sync = true }
			)
			local last
			local seen = {}
			local h = { path = path, session_id = id }
			function h.append(entry, opts)
				entry = D.store.copy_from_tool(entry)
				local sync = (opts and opts.sync) or entry.type == "message"
				return D.store.run_host(function()
					if entry.entry_id ~= nil then
						assert(
							type(entry.entry_id) == "string" and entry.entry_id ~= "",
							"child entry id must be a nonempty string"
						)
						assert(not seen[entry.entry_id], "duplicate child entry id: " .. entry.entry_id)
					else
						local eid
						repeat
							eid = mare.crypto.random_hex(8)
						until not seen[eid]
						entry.entry_id = eid
					end
					entry.parent_entry_id = entry.parent_entry_id == nil and last or entry.parent_entry_id
					entry.created_at_ms = entry.created_at_ms or lib.to_epoch_ms(mare.task.now())
					local _, lossy = file:append(entry, { sync = sync })
					D.store.warn_lossy_utf8(lossy)
					seen[entry.entry_id] = true
					last = entry.entry_id
				end)
			end
			local closed = false
			local dispose_writer
			local dispose_caller
			function h.close()
				if closed then
					return
				end
				closed = true
				local dw = dispose_writer
				dispose_writer = nil
				local dc = dispose_caller
				dispose_caller = nil
				if dw then
					pcall(dw)
				end
				if dc then
					pcall(dc)
				end
				return D.store.run_host(function()
					local f = file
					file = nil
					if f then
						pcall(
							f.append,
							f,
							{ type = "state", state = "closed", created_at_ms = lib.to_epoch_ms(mare.task.now()) },
							{ sync = true }
						)
						pcall(f.close, f)
					end
				end)
			end
			dispose_writer = ctx:effect(function()
				return function()
					if closed then
						return
					end
					-- Child disposers run before the store disposer. Mark interrupted publication
					-- first so closing a child cannot append a misleading clean marker.
					D.store.poison_interrupted_completion()
					h.close()
				end
			end, "session child writer")
			function h._attach_caller(disposer)
				if closed then
					if disposer then
						pcall(disposer)
					end
					return
				end
				dispose_caller = disposer
			end
			return h
		end, owner, close_abandoned)
		if not handle then
			return nil, err
		end
		-- The caller may be supplied by a tool; never invoke it on the host writer.
		-- Its registration can yield or fail, so retain ownership until it returns.
		local attached = false
		local guard <close> = setmetatable({}, {
			__close = function()
				if not attached then
					D.store.defer_host(function()
						handle.close()
					end)
				end
			end,
		})
		local caller_disposer = caller:effect(function()
			return handle.close
		end, "session child")
		handle._attach_caller(caller_disposer)
		handle._attach_caller = nil
		attached = true
		return handle
	end

	--- children(run_id?) -> { {path, run_id, name} } sub-session files (newest run first)
	function session.children(_, _, run_id)
		local out = {}
		local root = D.child_dir() .. "/agents"
		local runs = run_id and { { name = run_id, kind = "dir" } } or mare.fs.list(root)
		for _, r in ipairs(runs or {}) do
			if r.kind == "dir" then
				for _, f in ipairs(mare.fs.list(root .. "/" .. r.name) or {}) do
					if f.name:match("%.jsonl$") then
						out[#out + 1] =
							{ path = root .. "/" .. r.name .. "/" .. f.name, run_id = r.name, name = f.name }
					end
				end
			end
		end
		table.sort(out, function(a, b)
			return a.path > b.path
		end)
		return out
	end

	-- render a session file's entries as one node (messages, tool calls with their results)
	local function transcript_node(entries)
		local items, results = {}, {}
		for _, e in ipairs(entries) do
			local m = e.type == "message" and e.message
			if m and m.role == "tool_result" then
				results[m.tool_call_id or ""] = m
			end
		end
		local function text_of(content)
			if type(content) == "string" then
				return content
			end
			local parts = {}
			for _, b in ipairs(content or {}) do
				if b.type == "text" then
					parts[#parts + 1] = b.text
				end
			end
			return table.concat(parts, "\n")
		end
		for _, e in ipairs(entries) do
			local m = e.type == "message" and e.message
			if m and m.role == "user" then
				items[#items + 1] = ui.UserMessage:render(text_of(m.content))
				items[#items + 1] = ui.Gap(1)
			elseif m and m.role == "assistant" then
				local t = text_of(m.content)
				if t ~= "" then
					items[#items + 1] = ui.Markdown(t)
					items[#items + 1] = ui.Gap(1)
				end
				for _, b in ipairs(m.content or {}) do
					if b.type == "tool_call" then
						local arg = ""
						for _, v in pairs(b.arguments or {}) do
							if type(v) == "string" then
								arg = v:gsub("\n.*", "…")
								break
							end
						end
						local r = results[b.id or ""]
						local state = r == nil and "running" or (r.is_error and "error" or "ok")
						items[#items + 1] = ui.ToolCall:header(b.name .. (arg ~= "" and (" " .. arg) or ""), state)
						if r then
							items[#items + 1] = ui.ToolCall:body(ui.Clip(ui.Paragraph(text_of(r.content)), 6, true))
						end
					end
				end
			elseif e.type == "compaction" then
				items[#items + 1] = ui.Notice:render("(earlier conversation compacted)", "info")
			end
		end
		if #items == 0 then
			items[1] = ui.Line({ ui.Span("(empty)"):hl("MareDim") })
		end
		return ui.Lines(items)
	end

	local transcript_gen = 0
	local transcript_cancel = nil

	local function cancel_transcript_poller()
		transcript_gen = transcript_gen + 1
		if transcript_cancel then
			transcript_cancel:cancel()
			transcript_cancel = nil
		end
	end

	ctx:effect(function()
		return cancel_transcript_poller
	end)

	--- load_transcript(path, title?, opts?): show any session file — including one still being
	--- written (a running sub-agent) — in the full-screen transcript view, refreshed while open;
	--- `opts.keys` go to `ui.transcript` (the view's own keys in its header)
	function session.load_transcript(_, _, path, title, opts)
		title = title or (path:match("([^/]+)%.jsonl$") or path)
		cancel_transcript_poller()
		local gen = transcript_gen
		local cancel = mare.task.token()
		transcript_cancel = cancel
		local function show(refresh)
			-- a live file (a running sub-agent) may end in a half-written line; read-only view
			local ok, entries = pcall(mare.store.read, path, { lenient = true })
			if gen ~= transcript_gen or cancel:is_cancelled() then
				return false
			end
			if refresh and ui.transcript_title() ~= title then
				return false
			end
			ui.transcript(title, transcript_node(ok and entries or {}), opts)
			return true
		end
		if not show(false) then
			return
		end
		ctx:spawn(function()
			local last = mare.fs.stat(path)
			while gen == transcript_gen and not cancel:is_cancelled() do
				if not mare.task.sleep(cfg.transcript_poll_ms, cancel) then
					return
				end
				if gen ~= transcript_gen or cancel:is_cancelled() or ui.transcript_title() ~= title then
					return
				end
				local st = mare.fs.stat(path)
				if st and (not last or st.size ~= last.size or st.mtime ~= last.mtime) then
					last = st
					if not show(true) then
						return
					end
				end
			end
		end)
	end
end
