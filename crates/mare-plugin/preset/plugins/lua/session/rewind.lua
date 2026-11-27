-- session.rewind: abandoned-branch summaries, /undo's rewind helpers and
-- session:rewind_conversation (only with the commands plugin);
-- part of the `session` plugin (plugins/session.lua), loaded at the start of its apply.
return function(D)
	local ctx, S, session = D.ctx, D.S, D.session
	local new_id = D.store.new_id
	local path_to, ensure_full, abandoned_messages =
		D.history.path_to, D.history.ensure_full, D.history.abandoned_messages
	local touched_paths = D.snapshots.touched_paths
	local msg_text, own_prompt, relative_time = D.pickers.msg_text, D.pickers.own_prompt, D.pickers.relative_time

	local BRANCH_PROMPT = "The messages above are a branch of the conversation that the user is abandoning "
		.. "to continue from an earlier point. Summarize in a few bullet points what was tried, what was learned "
		.. "and what failed, so the new branch can benefit. Preserve exact file paths and error messages. Output only the summary."

	--- Build a `branch_summary` entry for the abandoned branch without mutating `S.leaf` or
	--- emitting `session/loaded`. Returns `(entry, nil)` on non-empty summary, `(false, nil)` on
	--- empty summary, or `(nil, err)` on failure/cancel/session-change.
	local function build_branch_summary(msgs, from_id, target_parent_id, opts)
		opts = opts or {}
		local owner = S.id
		if not D.follow.llm() then
			return nil, "llm service unavailable"
		end
		local prompt = BRANCH_PROMPT
		if opts.instructions and opts.instructions ~= "" then
			prompt = prompt .. "\n\nAdditional instructions from the user: " .. opts.instructions
		end
		if mare.ui.connected() then
			mare.ui.working("Summarizing abandoned branch")
		end
		local K = D.follow.compact()
		local summary, ferr
		if K and K.run then
			-- the compact service's budget and oldest-first protocol-unit dropping
			local ok_s, res = pcall(K.run, K, prompt, { summarize_only = true, messages = msgs })
			if ok_s and type(res) == "table" and res.summary ~= nil then
				summary = res.summary
				if (res.dropped or 0) > 0 then
					summary = summary
						.. ("\n\n_Note: the oldest %d message unit(s) did not fit in this summary; the originals are in the session log and can be recovered with /tree (source branch: %s)._"):format(
							res.dropped,
							tostring(from_id)
						)
					if mare.ui.connected() then
						ui.notify(
							("Branch summary: the %d oldest message unit(s) didn't fit the summary budget"):format(
								res.dropped
							),
							"info"
						)
					end
				end
			else
				local kind = type(res) == "table" and res.data and res.data.kind or nil
				ferr = kind == "cancelled" and "compaction cancelled"
					or (type(res) == "table" and res.message or tostring(res))
			end
		else
			local parts = {}
			for _, m in ipairs(msgs) do
				for _, b in ipairs(m.content or {}) do
					if b.type == "text" then
						parts[#parts + 1] = ("[%s]: %s"):format(m.role, b.text)
					elseif b.type == "tool_call" then
						parts[#parts + 1] = ("[%s tool call]: %s %s"):format(
							m.role,
							b.name,
							mare.json.encode(b.arguments or {})
						)
					end
				end
			end
			local res = D.follow.llm():complete({
				purpose = "branch_summary",
				system = "You summarize coding session branches precisely.",
				messages = {
					{
						role = "user",
						content = {
							{
								type = "text",
								text = "<conversation>\n"
									.. table.concat(parts, "\n\n")
									.. "\n</conversation>\n\n"
									.. prompt,
							},
						},
					},
				},
				tools = {},
				cache = false,
			})
			if res.stop_reason == "error" then
				ferr = tostring(res.error)
			else
				summary = D.follow.llm():text_of(res)
			end
		end
		if mare.ui.connected() then
			mare.ui.working(nil)
		end
		if S.id ~= owner or ferr == "compaction cancelled" then
			return nil, ferr or "session changed"
		end
		if opts.cancelled and opts.cancelled() then
			return nil, "compaction cancelled"
		end
		if not summary then
			local msg = tostring(ferr or "branch summary failed")
			ui.notify("Branch summary failed: " .. msg, "warning")
			return nil, msg
		end
		if summary == "" then
			return false
		end
		-- its own entry: where it came from and the files it touched,
		-- accumulated from earlier branch summaries on the abandoned path
		local files, seen = {}, {}
		for _, m in ipairs(msgs) do
			for _, f in ipairs(m.files or {}) do
				if not seen[f] then
					seen[f] = true
					files[#files + 1] = f
				end
			end
		end
		for _, m in ipairs(msgs) do
			for _, b in ipairs(m.content or {}) do
				if b.type == "tool_call" then
					for _, f in ipairs(touched_paths(b)) do
						if not seen[f] then
							seen[f] = true
							files[#files + 1] = f
						end
					end
				end
			end
		end
		return {
			type = "branch_summary",
			entry_id = new_id(),
			parent_entry_id = target_parent_id,
			from_entry_id = from_id,
			summary = summary,
			files = files,
			created_at_ms = require("mare_lib").to_epoch_ms(mare.task.now()),
		}
	end

	local user_text = msg_text

	--- when an entry was written (older entries may have no time)
	local function when(e)
		local t = e.created_at_ms or (e.message and e.message.timestamp)
		return type(t) == "number" and relative_time(t) or ""
	end

	-- /undo [count]: rewind the conversation — go back to before one of your recent prompts
	-- (a /tree branch: the original messages stay in the tree) and put its text back in the
	-- editor unless a draft is there. Files are not
	-- touched: /rewind (the `changes` plugin) restores files.
	local function rewind_to(e)
		local ok, err = session.set_leaf(session, ctx, e.parent_entry_id)
		if not ok then
			ui.notify("Not rewound: " .. tostring(err), "warning")
			return false
		end
		ui.set_editor(user_text(e.message), { keep_draft = true })
		return true
	end
	--- your prompts on the complete current path (oldest first)
	local function undoable()
		ensure_full()
		local list = {}
		for _, e in ipairs(path_to(S.leaf)) do
			if e.type == "message" and own_prompt(e.message) then
				list[#list + 1] = e
			end
		end
		return list
	end
	--- rewind_conversation(caller, before, {text?, summarize?}) -> true | nil, err: internal
	--- helper used by `session.rewind`. Refused while a turn runs.
	local function rewind_conversation(caller, before, opts)
		opts = opts or {}
		if ctx.agent:activity().busy then
			return nil, "a turn is running — interrupt it first"
		end
		ensure_full()
		local target = before or nil
		if target ~= nil and not S.entries[target] then
			return nil, "no such session entry: " .. tostring(target)
		end
		local abandoned = abandoned_messages(target)
		local from_leaf = S.leaf
		local owner = S.id
		if opts.summarize and #abandoned > 0 then
			local decision =
				ctx.policy:decide("session/before-tree", table.pack({ target_id = target, current_leaf = S.leaf }))
			if not decision or decision.decision ~= "allow" then
				return nil, (decision and decision.reason) or "tree navigation cancelled"
			end
			local entry, serr = build_branch_summary(abandoned, from_leaf, target, {})
			if S.id ~= owner then
				return nil, "session changed"
			end
			if entry == nil then
				if target ~= S.leaf then
					local ok, err = D.move_leaf(caller, target, { before_checked = true })
					if not ok then
						return nil, err
					end
				end
				if opts.text then
					ui.set_editor(opts.text, { keep_draft = true })
				end
				local reason = tostring(serr or "branch summary failed")
				return nil,
					"rewound, but branch summary failed: " .. reason,
					{
						rewound = true,
						leaf_entry_id = S.leaf,
						summarize_error = reason,
					}
			end
			if type(entry) == "table" or target ~= S.leaf then
				local ok, err = D.move_leaf(caller, target, {
					summary_entry = type(entry) == "table" and entry or nil,
					before_checked = true,
				})
				if not ok then
					return nil, err
				end
			end
			if opts.text then
				ui.set_editor(opts.text, { keep_draft = true })
			end
			return true
		end
		if target ~= S.leaf then
			local ok, err = D.move_leaf(caller, target)
			if not ok then
				return nil, err
			end
		end
		if opts.text then
			ui.set_editor(opts.text, { keep_draft = true })
		end
		return true
	end

	return {
		build_branch_summary = build_branch_summary,
		user_text = user_text,
		when = when,
		rewind_to = rewind_to,
		undoable = undoable,
		rewind_conversation = rewind_conversation,
	}
end
