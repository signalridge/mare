-- compact.stages.commit: atomic commit of a summary, stored-summary stripping and the
-- recovery footer; part of the lazy `compact.stages` module (lua/compact/stages.lua),
-- loaded at the start of its factory, i.e. on the first compaction.
return function(D)
	local ctx, changed = D.ctx, D.changed
	local S = D.S
	local id_of, session_path = D.summarize.id_of, D.summarize.session_path

	--- commit a summary stage's result atomically: persisted first, then swapped in
	local function commit(summary_text, kept, tokens_before, details, extra)
		if not D.unchanged() then
			return nil, "the conversation changed during compaction; result discarded"
		end
		local ag = ctx.agent
		local snapshot = ag:messages()
		local snapshot_hash = mare.crypto.sha256(mare.json.encode(snapshot))
		local function unchanged()
			return D.unchanged() and mare.crypto.sha256(mare.json.encode(ag:messages())) == snapshot_hash
		end
		local tx = D.S.transaction
		if tx then
			extra.projections = tx.projections
			if extra.range then
				extra.range = { D.original(extra.range[1]), D.original(extra.range[2]) }
			end
		end
		local prepared_overhead = D.overhead(tx and tx.target_model)
		local function publish(new)
			if tx then
				tx.committed, tx.messages = true, new
			end
			ctx.agent:set_messages(new, { notify = false })
			if tx and tx.publish then
				tx.publish()
			end
			changed(new, prepared_overhead, false)
		end
		extra.validate = unchanged
		extra.publish = publish
		extra.model = tx and tx.model or nil
		local new
		if S.svc.session and not (tx and not tx.publishing) then
			local err
			new, err = S.svc.session:append_compaction(summary_text, D.original(kept[1]), tokens_before, details, extra)
			if not new then
				return nil, err
			end
		else
			new = {}
			for _, pm in ipairs(extra.preserved or {}) do
				local m = table.copy(pm)
				m.preserved = true
				new[#new + 1] = m
			end
			new[#new + 1] = {
				role = "user",
				compaction = true,
				summary = summary_text,
				details = details,
				method = extra.method,
				remote = extra.remote,
				content = {
					{
						type = "text",
						text = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n"
							.. summary_text
							.. "\n</summary>",
					},
				},
			}
			for _, m in ipairs(kept) do
				new[#new + 1] = m
			end
		end
		if tx and not tx.publishing then
			tx.messages = new
			tx.commit = function(publish)
				tx.publishing, tx.publish = true, publish
				local result, err = commit(summary_text, kept, tokens_before, details, extra)
				if not result then
					error(err, 0)
				end
				return result
			end
			return new
		end
		if not S.svc.session then
			mare.task.publish(function()
				assert(unchanged(), "the conversation changed during compaction; result discarded")
				publish(new)
			end)
		end
		D.notify(new)
		return new
	end

	--- the model's part of a stored summary: what mare appended (dropped-range note, file lists,
	--- todo list, recovery footer) and the compaction wrapper come off before it is reused as
	--- <previous-summary> — otherwise each compaction carries them into the next and mare
	--- appends them once more
	local function strip_summary(text)
		text = tostring(text or "")
		if text:find("^The conversation history before this point was compacted") then
			text = text:match("<summary>\n(.*)\n</summary>%s*$") or text
		end
		text = text:gsub("\n\n<recovery>\n.*$", "")
		text = text:gsub("\n\n## Current Todo List\n.*$", "")
		text = text:gsub("\n\n<modified%-files>\n.-\n</modified%-files>", "")
		text = text:gsub("\n\n<read%-files>\n.-\n</read%-files>", "")
		text = text:gsub("\n\n_Note: the earliest %d+ message%(s%).-/tree%._", "")
		return text
	end

	--- Context recovery footer pointing to mare's session log.
	local function recovery_footer(first, last)
		local a, b = first and id_of(first), last and id_of(last)
		local where = session_path()
		if not (a and b and where) then
			return "\n\n<recovery>\nThe compacted messages are kept in this session's log (not saved to disk in this session). "
				.. "Ask the user if you need their exact wording.\n</recovery>"
		end
		return (
			"\n\n<recovery>\n## Context Recovery\nEverything before this note is still on disk in this session's log "
			.. "(read-only, append-only):\n  %s\n  summarized: entries %s … %s\n"
			.. "If you need exact command output, file contents, error text, or the wording of an earlier request, "
			.. "look it up there instead of guessing. Format: one JSON entry per line with `entry_id` and `parent_entry_id`; the "
			.. 'conversation is in `type = "message"` entries; `compaction` and `prune` entries mark earlier '
			.. "compaction (older lines stay in the file). Lines can be long: grep for a keyword or an id, then read "
			.. "only that line.\n</recovery>"
		):format(where, a, b)
	end

	return { commit = commit, strip_summary = strip_summary, recovery_footer = recovery_footer }
end
