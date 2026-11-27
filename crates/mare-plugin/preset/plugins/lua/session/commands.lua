-- session.commands: the session slash commands (/new /resume /retry /name /undo /tree
-- /fork /clone /label /archive /delete /continue /session);
-- part of the `session` plugin (plugins/session.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg, S, session = D.ctx, D.cfg, D.S, D.session
	local add = D.store.add
	local function path_to(...)
		return D.history.path_to(...)
	end
	local function abandoned_messages(...)
		return D.history.abandoned_messages(...)
	end
	local function ensure_full(...)
		return D.history.ensure_full(...)
	end
	local function index_update(...)
		return D.history.index_update(...)
	end
	local function session_svc()
		return assert(ctx:get("session"), "session service unavailable")
	end
	local function own_prompt(...)
		return D.pickers.own_prompt(...)
	end
	local function refuse_busy(...)
		return D.pickers.refuse_busy(...)
	end
	local function search_sessions(...)
		return D.pickers.search_sessions(...)
	end
	local function resolve_resume(...)
		return D.pickers.resolve_resume(...)
	end
	local function pick_session(...)
		return D.pickers.pick_session(...)
	end
	local function build_branch_summary(...)
		return D.rewind.build_branch_summary(...)
	end
	local function move_leaf(...)
		return D.move_leaf(...)
	end
	local function user_text(...)
		return D.rewind.user_text(...)
	end
	local function when(...)
		return D.rewind.when(...)
	end
	local function rewind_to(...)
		return D.rewind.rewind_to(...)
	end
	local function undoable(...)
		return D.rewind.undoable(...)
	end

	local reg = function(n, d)
		ctx.commands:register(n, d)
	end
	local alias = function(n, target)
		ctx.commands:register(n, { alias_of = target })
	end
	local ended, new_pending = 0, false
	ctx:on("turn/completed", function()
		ended = ended + 1
	end)
	reg("new", {
		description = "Start a new session",
		run = function()
			if new_pending then
				ui.notify("A new session is already waiting for the current turn to stop", "warning")
				return
			end
			local was_busy = ctx.agent:activity().busy
			local turn_active = D.store.turn_active()
			local side_pending = D.store.requests_pending(S.id)
			if turn_active or side_pending then
				local old_id, old_end = S.id, ended
				new_pending = true
				local aborted, abort_err = true, nil
				if was_busy then
					aborted, abort_err = pcall(ctx.agent.cancel, ctx.agent)
				end
				if not aborted then
					new_pending = false
					ui.notify("Could not cancel the previous turn: " .. tostring(abort_err), "warning")
					return
				end
				ctx:spawn(function()
					local ok, err = pcall(function()
						local drained, why = D.store.drain_side(function()
							return (was_busy and ended == old_end) or D.store.turn_active()
						end, old_id)
						if not drained then
							return why
						end
						session_svc():create()
					end)
					new_pending = false
					if not ok then
						ui.notify(
							"Could not start a new session: " .. tostring(type(err) == "table" and err.message or err),
							"warning"
						)
					elseif err then
						ui.notify(err, "warning")
					end
				end)
				return
			end
			session_svc():create()
		end,
	})
	reg("resume", {
		description = "Resume a session (/resume all · /resume search <text>)",
		-- refused while a turn runs (commands availability; the check inside stays for
		-- callers of run() and for pickers that return after a turn started)
		availability = { "idle", "session" },
		run = function(args)
			if refuse_busy("/resume") then
				return
			end
			ctx:spawn(function()
				local q = string.match(args, "^search%s+(.+)$")
				if q then
					search_sessions(q)
					return
				end
				if args ~= "" and args ~= "all" then
					local p, known = resolve_resume(args, true)
					if p then
						local svc = session_svc()
						local ok, err = pcall(svc.load, svc, { path = p })
						if not ok then
							ui.notify(type(err) == "table" and err.message or tostring(err), "warning")
						end
					else
						ui.notify(known or ("No session matches " .. args), "warning")
					end
					return
				end
				pick_session(args == "all")
			end)
		end,
	})
	-- /retry: regenerate the last answer — branch the session tree at the last
	-- user message on the current path and send it again; the old answer stays on its
	-- branch (/tree). No reference has the command; it is the existing tree fork.
	reg("retry", {
		description = "Regenerate the last answer (the old one stays on its branch: /tree)",
		availability = { "idle", "session" },
		run = function(_, input)
			if refuse_busy("/retry") then
				return
			end
			ensure_full()
			local last
			for _, e in ipairs(path_to(S.leaf)) do
				local m = e.type == "message" and e.message
				if m and m.role == "user" and not m.compaction and not m.turn_aborted and not m.notification then
					last = e
				end
			end
			if not last then
				ui.notify("Nothing to retry")
				return
			end
			local copy_deep = require("mare_lib").copy_deep
			local parts = {}
			for _, b in ipairs(last.message.content or {}) do
				if b.type == "text" then
					parts[#parts + 1] = { type = "text", text = b.text }
				elseif b.type == "mention" then
					parts[#parts + 1] = {
						type = "file",
						path = b.path,
						mime = b.mime,
						name = b.name,
					}
				elseif b.type == "image" then
					parts[#parts + 1] = {
						type = "image",
						mime = b.mime,
						data = b.data,
						path = b.path,
					}
				end
			end
			local moved, merr = session.set_leaf(session, ctx, last.parent_entry_id)
			if not moved then
				ui.notify("Not retried: " .. tostring(merr), "warning")
				return
			end
			ctx.bus:emit("session/retry", { from = last.entry_id })
			local input_context = (type(input) == "table" and input.input_context ~= nil) and input.input_context
				or last.message.input_context
			local prompt_input = #parts > 0 and parts or ""
			local prompt_opts = {
				source = last.message.source or "untrusted",
				input_context = input_context ~= nil and copy_deep(input_context) or nil,
			}
			ctx:spawn(function()
				ctx.agent:prompt(prompt_input, prompt_opts)
			end)
		end,
	})
	alias("clear", "new")
	alias("sessions", "resume")
	alias("title", "name")
	alias("rename", "name")
	reg("name", {
		description = "Name this session (/name <title>)",
		run = function(args)
			if args == "" then
				ui.notify("Session: " .. (S.name or "(unnamed)"))
				return
			end
			local ok, err = D.set_name(args)
			ui.notify(
				ok and ("Session named: " .. args) or ("Not named: " .. tostring(err)),
				ok and "info" or "warning"
			)
		end,
	})

	reg("undo", {
		description = "Rewind the conversation to before a prompt (/undo picks one; /undo N: the Nth-last); files stay — /rewind restores them",
		availability = { "idle", "session" },
		run = function(args)
			if refuse_busy("/undo") then
				return
			end
			ctx:spawn(function()
				ensure_full()
				local list = undoable()
				if #list == 0 then
					ui.notify("Nothing to undo")
					return
				end
				local n = tonumber(args)
				if not n and not ui.connected() then
					n = 1 -- no picker without a UI (headless / RPC): one step back
				end
				if n then
					-- /undo N: fork at the Nth-last user message
					n = math.max(1, math.floor(n))
					if n > #list then
						ui.notify(("Only %d prompt%s to undo"):format(#list, #list == 1 and "" or "s"), "warning")
						return
					end
					rewind_to(list[#list - n + 1])
					return
				end
				-- no count: pick how far back, newest first
				local items = {}
				for i = #list, 1, -1 do
					local e = list[i]
					items[#items + 1] = {
						label = mare.text.trunc_width((string.gsub(user_text(e.message), "%s+", " ")), 76),
						detail = (#list - i + 1 == 1 and "last prompt" or (#list - i + 1) .. " back")
							.. (when(e) ~= "" and (" · " .. when(e)) or ""),
					}
				end
				local i = ui.select({
					title = "Rewind the conversation to before which prompt? (files stay — /rewind)",
					items = items,
				})
				if i and not refuse_busy("/undo") then
					rewind_to(list[#list - i + 1])
				end
			end)
		end,
	})

	-- /tree: every node of the session tree — your messages, answers,
	-- tool calls and results, compactions, branch summaries — drawn as the real tree (a
	-- chain stays flat; branches get connectors), foldable, with filters, labels and times
	local function tool_names(m)
		local names = {}
		for _, b in ipairs(m.content or {}) do
			if b.type == "tool_call" then
				names[#names + 1] = b.name
			end
		end
		return names
	end

	reg("tree", {
		description = "Navigate the session tree (branch from any earlier point)",
		-- the view opens any time; its actions refuse while a turn runs (the session's own check)
		availability = "session",
		-- the tree view loads on first use
		run = function()
			return require("session.tree")({
				ctx = ctx,
				S = S,
				cfg = cfg,
				session = session,
				path_to = path_to,
				abandoned_messages = abandoned_messages,
				refuse_busy = refuse_busy,
				build_branch_summary = build_branch_summary,
				move_leaf = move_leaf,
				ensure_full = ensure_full,
				when = when,
				own_prompt = own_prompt,
				user_text = user_text,
				add = add,
				index_update = index_update,
				tool_names = tool_names,
				-- the optional services session follows (llm for the branch summary)
				follow = D.follow,
			})
		end,
	})
	--- fork at `e` (a message of yours): a new session holding what came before it, the message
	--- back in the input to send again or edit
	local function fork_at(e)
		session_svc():fork({ session_id = S.id, before_item_id = e.entry_id })
		ui.set_editor(user_text(e.message), { keep_draft = true })
		ui.notify("Forked to new session")
	end
	local function refuse_readonly_fork()
		if S.readonly and S.lenient and S.path then
			ui.notify("Cannot fork a damaged session safely; back up or repair the source first", "warning")
			return true
		end
		return false
	end

	reg("fork", {
		description = "Fork into a new session from your latest message (/fork pick: choose one)",
		args = "[pick]",
		availability = { "idle", "session" },
		run = function(args)
			if refuse_busy("/fork") or refuse_readonly_fork() then
				return
			end
			-- no argument: the latest message of yours on this branch (the message comes back into
			-- the input)
			if args == nil or args == "" then
				ensure_full()
				local path = path_to(S.leaf)
				for k = #path, 1, -1 do
					local e = path[k]
					if e.type == "message" and own_prompt(e.message) then
						return fork_at(e)
					end
				end
				ui.notify("Nothing to fork yet")
				return
			elseif args ~= "pick" then
				ui.notify("Usage: /fork (from your latest message) or /fork pick", "warning")
				return
			end
			ctx:spawn(function()
				ensure_full()
				-- every message of yours in the tree, not just the current branch
				local on_path = {}
				for _, e in ipairs(path_to(S.leaf)) do
					on_path[e.entry_id] = true
				end
				local list = {}
				for _, e in ipairs(S.list) do
					if e.type == "message" and e.entry_id and S.entries[e.entry_id] == e and own_prompt(e.message) then
						list[#list + 1] = e
					end
				end
				if #list == 0 then
					ui.notify("Nothing to fork yet")
					return
				end
				local items, sel = {}, #list
				for i, e in ipairs(list) do
					items[i] = {
						label = (on_path[e.entry_id] and "● " or "○ ")
							.. mare.text.trunc_width((string.gsub(user_text(e.message), "%s+", " ")), 76),
						detail = (on_path[e.entry_id] and "" or "other branch · ") .. when(e),
					}
					if on_path[e.entry_id] then
						sel = i
					end
				end
				local i = ui.select({
					title = "Fork from message (● this branch, ○ another)",
					items = items,
					selected = sel,
				})
				if not i or refuse_busy("/fork") then
					return
				end
				fork_at(list[i])
			end)
		end,
	})
	reg("clone", {
		description = "Duplicate this session into a new one",
		availability = { "idle", "session" },
		run = function()
			if refuse_busy("/clone") or refuse_readonly_fork() then
				return
			end
			session_svc():fork({ session_id = S.id })
			ui.notify("Cloned session")
		end,
	})
	reg("label", {
		description = "Label the current point in the session tree (/label <text>; empty clears)",
		availability = { "idle", "session" },
		run = function(args)
			if refuse_busy("/label") then
				return
			end
			if not S.leaf then
				ui.notify("Nothing to label yet")
				return
			end
			local ok, err = add({
				type = "label",
				target_entry_id = S.leaf,
				label = args,
				created_at_ms = require("mare_lib").to_epoch_ms(mare.task.now()),
			})
			if not ok then
				ui.notify("Label not saved: " .. tostring(err), "warning")
				return
			end
			S.labels[S.leaf] = args ~= "" and args or nil
			index_update()
			ui.notify(args ~= "" and ("Labeled: " .. args) or "Label cleared")
		end,
	})
	reg("archive", {
		description = "Archive this session and start a new one",
		run = function()
			if not S.path or not mare.fs.exists(S.path) or not S.file then
				ui.notify("Nothing saved to archive")
				return
			end
			if ctx.agent:activity().busy then
				ctx.agent:cancel()
				ctx.agent:cancel()
			end
			local name = S.name or string.match(S.path, "([^/\\]+)$")
			local prev_id, prev_generation, prev_path, prev_file = S.id, S.generation, S.path, S.file
			local ts = require("mare_lib").to_epoch_ms(mare.task.now())
			-- Explicitly switch away under the same lock (`close_entries` writes archive before release)
			-- since `session.archive` is a host index operation with no implicit switch.
			local switched, serr = pcall(D.lifecycle.new_session, {
				expected_session = {
					id = prev_id,
					generation = prev_generation,
					path = prev_path,
					file = prev_file,
				},
				close_entries = {
					{ type = "archive", archived = true, created_at_ms = ts },
				},
			})
			if not switched then
				ui.notify("Not archived: " .. tostring(serr), "warning")
				return
			end
			local iok, ierr = pcall(mare.store.index_append, { path = prev_path })
			if not iok then
				mare.log("warn", "session index not updated: " .. tostring(ierr))
			end
			local failures = D.metadata.archive_children(prev_id, true, ts)
			if #failures > 0 then
				ui.notify(
					"Archived " .. name .. " (" .. D.metadata.format_child_failures(failures, "archived") .. ")",
					"warning"
				)
			else
				ui.notify("Archived " .. name .. " (/resume, Ctrl+A lists archived sessions)")
			end
		end,
	})
	reg("delete", {
		description = "Delete this session, or delete a session file by path",
		args = "[path]",
		run = function(args)
			ctx:spawn(function()
				local requested = (args or ""):match("^%s*(.-)%s*$")
				if requested ~= "" then
					local ok, path = pcall(mare.fs.resolve, requested)
					if not ok or not path or not mare.fs.exists(path) then
						ui.notify("Session file not found: " .. requested, "warning")
						return
					end
					local current = S.path and mare.fs.exists(S.path) and mare.fs.resolve(S.path)
					if path ~= current then
						if not ui.confirm(("Delete session file %q permanently?"):format(path), false) then
							return
						end
						local deleted, err = pcall(session.delete, session, ctx, path)
						if deleted then
							ui.notify("Deleted session file " .. path)
						else
							ui.notify(
								"Session not fully deleted: " .. tostring(type(err) == "table" and err.message or err),
								"warning"
							)
						end
						return
					end
				end
				if not S.path or not mare.fs.exists(S.path) then
					ui.notify("Nothing saved to delete")
					return
				end
				if not ui.confirm("Delete this session permanently?", false) then
					return
				end
				local p, id = S.path, S.id
				if ctx.agent:activity().busy then
					ctx.agent:cancel()
					ctx.agent:cancel()
				end
				local svc = session_svc()
				if ctx.kernel:session_capacity() == "multi" then
					-- In multi-runtime mode, `svc:close(id)` shuts down this host/VM, so require
					-- an idle session, arm the delete barrier while holding the switch lease, and
					-- remove the files while the VM is still alive. Failed deletes disarm the
					-- barrier and roll back cleanly without destroying the active runtime.
					local ok, err
					do
						local lease_ok, lease = pcall(D.store.begin_switch)
						if not lease_ok then
							ui.notify(
								"Session not deleted: " .. tostring(type(lease) == "table" and lease.message or lease),
								"warning"
							)
							return
						end
						local _lease_guard <close> = lease
						local prep_ok, prepared, prep_err = pcall(D.store.begin_delete, id)
						if not prep_ok or not prepared then
							local msg = not prep_ok and prepared or prep_err
							ui.notify(
								"Session not deleted: " .. tostring(type(msg) == "table" and msg.message or msg),
								"warning"
							)
							return
						end
						local del_ok, del_res, del_err = pcall(D.metadata.delete_and_emit, p, id)
						if del_ok and del_res then
							ok = true
						else
							ok = false
							err = not del_ok and del_res or del_err
						end
					end
					if ok then
						ui.clear()
						ui.notify("Deleted session")
						local close_ok, close_err = pcall(svc.close, svc, id)
						if not close_ok then
							ui.notify(
								"Deleted session, but failed to unload runtime: "
									.. tostring(type(close_err) == "table" and close_err.message or close_err),
								"warning"
							)
						end
					else
						D.store.cancel_delete()
						local msg = tostring(type(err) == "table" and err.message or err)
						local rok, rerr = pcall(D.lifecycle.load_file, p, nil, { skip_policy = true })
						if not rok then
							ui.notify(
								"Session not deleted: " .. msg .. " (rollback failed: " .. tostring(rerr) .. ")",
								"warning"
							)
						else
							ui.notify("Session not fully deleted: " .. msg, "warning")
						end
					end
					return
				end
				-- Explicitly close first via the session service / orchestrator: `session.delete`
				-- never switches implicitly and cannot remove an open session journal while this
				-- process holds its lock.
				local switched, serr = pcall(svc.close, svc, id)
				if not switched then
					ui.notify(
						"Session not deleted: " .. tostring(type(serr) == "table" and serr.message or serr),
						"warning"
					)
					return
				end
				local ok, err = pcall(session.delete, session, ctx, p or id)
				if ok then
					ui.notify("Deleted session")
				else
					local msg = tostring(type(err) == "table" and err.message or err)
					-- Rollback bypasses `session/before-switch` policy (`skip_policy` is internal to
					-- the built-in session provider and intentionally not exposed on `session.load`).
					local rok, rerr = pcall(D.lifecycle.load_file, p, nil, { skip_policy = true })
					if not rok then
						ui.notify(
							"Session not deleted: " .. msg .. " (rollback failed: " .. tostring(rerr) .. ")",
							"warning"
						)
					else
						ui.notify("Session not fully deleted: " .. msg, "warning")
					end
				end
			end)
		end,
	})
	reg("continue", {
		description = "Continue an interrupted turn (after a crash or kill)",
		availability = { "idle", "session" },
		run = function()
			ctx:spawn(function()
				local ok, err = pcall(ctx.agent.continue, ctx.agent)
				if not ok and type(err) == "table" and err.code == "agent.no_pending_work" then
					ui.notify("Nothing to continue: the last turn completed", "info")
				elseif not ok then
					ui.notify(tostring(type(err) == "table" and err.message or err), "warning")
				end
			end)
		end,
	})

	reg("session", {
		description = "Show session info (/session id copies the id)",
		run = function(args)
			if args == "id" then
				local copy = ctx:get("copy")
				if copy and copy.text and type(S.id) == "string" and S.id ~= "" then
					ctx:spawn(function()
						pcall(copy.text, copy, S.id, "the session-id message")
					end)
				end
				ui.notify("Session id: " .. tostring(S.id))
				return
			end
			local lines = {
				ui.Line({ ui.Span("id      "):hl("MareMuted"), S.id or "" }),
				ui.Line({ ui.Span("name    "):hl("MareMuted"), S.name or "(unnamed)" }),
				ui.Line({ ui.Span("file    "):hl("MareMuted"), S.path or "(not saved yet)" }),
				ui.Line({ ui.Span("entries "):hl("MareMuted"), tostring(#S.list) }),
			}
			ui.overlay("Session", ui.Lines(lines))
		end,
	})
end
