-- compact.stages.local_summary: preserved user messages and the local (structured summary)
-- stage, stored into the shared stage table; part of the lazy `compact.stages` module (lua/compact/stages.lua),
-- loaded at the start of its factory, i.e. on the first compaction.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local S = D.S
	local msg_tokens, estimate = D.msg_tokens, D.estimate
	local P = D.prompts
	local SYSTEM, SUMMARIZE, UPDATE, TURN_PREFIX = P.SYSTEM, P.SUMMARIZE, P.UPDATE, P.TURN_PREFIX
	local Z = D.summarize
	local text_of, cp_head, id_of, units_of, summarize, repair_tool_order =
		Z.text_of, Z.cp_head, Z.id_of, Z.units_of, Z.summarize, Z.repair_tool_order
	local cut_point, turn_start, track_files = D.cut.cut_point, D.cut.turn_start, D.cut.track_files
	local persist_projection, offload_plan, stage_impl = D.persist_projection, D.offload_plan, D.stage_impl
	local commit, strip_summary, recovery_footer = D.commit.commit, D.commit.strip_summary, D.commit.recovery_footer
	local SUMMARY_PREFIX =
		"The conversation history before this point was compacted into the following summary:\n\n<summary>\n"
	local SUMMARY_SUFFIX = "\n</summary>"
	local TRUNCATED = "\n[... truncated to fit the compacted context ...]"

	--- a verbatim-retention budget (keep_recent_tokens, recent_user_tokens) capped at
	--- retain_ratio × the message budget.
	local function retain(n)
		local budget = D.budgets()
		return budget and math.min(n, math.floor(budget.message * cfg.retain_ratio)) or n
	end

	--- user messages kept verbatim, newest first: while within `budget` (`recent_user_tokens`, see
	--- retain); the one that doesn't fit is truncated to what is left, older ones live on in the
	--- summary only. Candidates are the user messages of the summarized range and those an
	--- earlier compaction kept, so a second compaction doesn't lose them.
	local function preserve_users(msgs, start, cut, budget, max_count)
		local cands = {}
		for i = cut - 1, start, -1 do
			local m = msgs[i]
			if m.role == "user" and not m.compaction and not m.preserved and not m.context_disclosure then
				cands[#cands + 1] = m
			end
		end
		for i = start - 1, 1, -1 do
			if msgs[i].preserved and msgs[i].role == "user" then
				cands[#cands + 1] = msgs[i]
			end
		end
		local preserved, remaining = {}, budget
		for _, m in ipairs(cands) do
			if remaining <= 0 then
				break
			end
			do
				local t = msg_tokens(m)
				if t <= remaining then
					table.insert(preserved, 1, m)
					remaining = remaining - t
				else
					local text = text_of(m)
					local n = math.max(1, remaining * 3)
					local cut_text = cp_head(text, n)
					local suffix = TRUNCATED
					while
						n > 0
						and msg_tokens({ content = { { type = "text", text = cut_text .. suffix } } }) > remaining
					do
						n = n * 3 // 4
						cut_text = cp_head(text, n)
					end
					if n == 0 then
						break
					end
					local c = table.copy(m)
					c.content = {
						{ type = "text", text = cut_text .. suffix },
					}
					table.insert(preserved, 1, c)
					break
				end
			end
			if max_count and #preserved >= max_count then
				break
			end
		end
		return preserved
	end

	local function minimal_users(msgs, start, cut)
		local latest = preserve_users(msgs, start, cut, math.huge, 1)[1]
		if not latest then
			return {}
		end
		local smallest =
			msg_tokens({ content = { { type = "text", text = cp_head(text_of(latest), 1) .. TRUNCATED } } })
		return preserve_users(msgs, start, cut, smallest, 1)
	end

	--- Select one tier using a lower bound before any summary request.
	local function plan_candidate(st)
		local raw = D.messages()
		if #raw == 0 then
			return nil, "nothing to compact"
		end
		local msgs = repair_tool_order(raw)
		if #msgs == 0 then
			return nil, "nothing to compact"
		end
		-- Even an empty summary has this envelope. If it and the fixed request overhead
		-- already exceed the target, no retained-history choice can possibly converge.
		local empty_envelope = SUMMARY_PREFIX .. SUMMARY_SUFFIX
		local minimum = D.overhead() + msg_tokens({ content = { { type = "text", text = empty_envelope } } })
		-- mid-turn: the agent is
		-- inside a turn, between its tool results and the next request. The model is trained to
		-- see the summary as the last item then, so nothing is kept after it: the whole turn goes
		-- into the summary and its prompt stays verbatim among the preserved user messages,
		-- before the summary. Mare's initial context (system prompt, AGENTS.md, skills, tools) is
		-- not part of the history but sent with every request, so it is in place already.
		local mid_turn = st.reason ~= "manual"
			and msgs[#msgs].role == "tool_result"
			and ctx.agent.activity ~= nil
			and ctx.agent:activity().busy == true
		local attempt = st.local_attempt or 1
		local scale = attempt == 1 and 1 or (attempt == 2 and 0.5 or 0)
		local cut = (mid_turn or scale == 0) and #msgs + 1
			or cut_point(msgs, math.floor(retain(cfg.keep_recent_tokens) * scale))
		local prev, start = nil, 1
		while start <= #msgs and (msgs[start].compaction or msgs[start].preserved) do
			if msgs[start].compaction then
				prev = msgs[start]
			end
			start = start + 1
		end
		local force_portable_remote = D.S.target_model ~= nil
			and type(prev) == "table"
			and type(prev.remote) == "table"
			and (prev.remote.provider_id ~= D.S.target_model.provider or prev.remote.model_id ~= D.S.target_model.id)
		if not cut and prev then
			cut = #msgs + 1
		end
		if not cut or cut <= 1 then
			-- Manual /compact preserves the previous no-op when the first retention budget
			-- contains the whole conversation. Automatic pressure recovery may still shrink it.
			if st.reason == "manual" and attempt == 1 then
				return nil, "nothing to compact"
			end
			if attempt < 3 then
				st.local_attempt = attempt + 1
				return nil, nil, "retry"
			end
			return nil, "nothing to compact"
		end
		-- A repeated manual compact with no new history is a no-op unless the old summary
		-- itself still leaves the request above the target (or a switch needs portable text).
		if
			not force_portable_remote
			and prev
			and start > #msgs
			and estimate(msgs) + D.overhead() <= D.threshold() * 0.9
		then
			return nil, "nothing to compact"
		end
		if
			not force_portable_remote
			and cut <= start
			and (not prev or estimate(msgs) + D.overhead() <= D.threshold() * 0.9)
		then
			if st.reason == "manual" and attempt == 1 then
				return nil, "nothing to compact"
			end
			if not prev and attempt < 3 then
				st.local_attempt = attempt + 1
				return nil, nil, "retry"
			end
			return nil, "nothing to compact"
		end
		if minimum > D.threshold() * 0.9 then
			st.exhausted = true
			return nil, "fixed request overhead leaves no room for a summary below the target"
		end
		-- Select only tiers whose fixed cost fits even with an empty summary. The final
		-- tier uses a minimal latest-user message: an optimistic feasibility bound.
		local preview_kept = {}
		for i = cut, #msgs do
			preview_kept[#preview_kept + 1] = msgs[i]
		end
		local preview_users
		if attempt == 3 then
			preview_users = minimal_users(msgs, start, cut)
		else
			preview_users = preserve_users(msgs, start, cut, math.floor(retain(cfg.recent_user_tokens) * scale))
		end
		-- Offloading a summarized image can make a preserved user message smaller.
		-- Until that projection is known, zero is the safe lower bound for its users.
		local may_offload_image = false
		for i = start, cut - 1 do
			if msgs[i].role == "user" then
				for _, block in ipairs(msgs[i].content or {}) do
					if block.type == "image" then
						may_offload_image = true
					end
				end
			end
		end
		local minimum_candidate = estimate(preview_kept)
			+ (may_offload_image and 0 or estimate(preview_users))
			+ msg_tokens({ content = { { type = "text", text = empty_envelope } } })
		if
			minimum_candidate + D.overhead() > D.threshold() * 0.9
			or (not force_portable_remote and minimum_candidate >= estimate(msgs))
		then
			if attempt < 3 then
				st.local_attempt = attempt + 1
				return nil, nil, "retry"
			end
			st.exhausted = true
			return nil, "no retention tier can fit below the target"
		end
		return {
			raw = raw,
			msgs = msgs,
			mid_turn = mid_turn,
			attempt = attempt,
			scale = scale,
			cut = cut,
			prev = prev,
			start = start,
			force_portable_remote = force_portable_remote,
		}
	end

	--- Evaluate one retention tier. The outer loop selects or retries tiers without
	--- recursively re-entering the stage.
	local function local_candidate(st)
		local function failed(err, kind)
			if kind == "cancelled" or kind == "policy" then
				st.stop = err
				return false, err
			end
			-- A provider/request failure is not evidence that the context cannot converge.
			-- The auto-compaction caller applies its normal backoff before trying again.
			return false, err
		end
		local plan, err, action = plan_candidate(st)
		if not plan then
			return false, err, action
		end
		local msgs, mid_turn, attempt, scale, cut, prev, start =
			plan.msgs, plan.mid_turn, plan.attempt, plan.scale, plan.cut, plan.prev, plan.start
		local split = cut <= #msgs and not mid_turn and msgs[cut].role ~= "user"
		local history_end, turn = cut - 1, nil
		if split then
			local ts = turn_start(msgs, cut)
			if ts and ts >= start then
				history_end = ts - 1
				turn = {}
				for i = ts, cut - 1 do
					turn[#turn + 1] = msgs[i]
				end
			end
		end
		local history = {}
		for i = start, history_end do
			history[#history + 1] = msgs[i]
		end
		if #history == 0 and not turn and not prev then
			return false, "nothing to compact"
		end
		-- images inside the summarized range are written out first (never silently dropped)
		local omap, on, offload_failed = offload_plan(msgs, 0, start, cut - 1)
		if offload_failed > 0 then
			st.stop = ("failed to offload %d image(s) before summarizing"):format(offload_failed)
			return false, st.stop
		end
		if on > 0 then
			local new, err = persist_projection(omap, on, "offload", plan.raw)
			if not new then
				st.stop = err or "failed to persist image offload"
				return false, st.stop
			end
			st.local_cache = nil
			return nil, nil, "restart" -- recompute on the offloaded context
		end

		local snapshot = table.copy(plan.raw)
		local snapshot_hash_ok, snapshot_hash = pcall(function()
			return mare.crypto.sha256(mare.json.encode(plan.raw))
		end)
		local focus = (st.instructions and st.instructions ~= "") and ("\n\nAdditional focus: " .. st.instructions)
			or ""
		-- the protected prefix: preserved user messages — replayed as is (prompt cache) and never
		-- dropped. The earlier summary goes in once, as <previous-summary> in the instruction,
		-- not also as the compaction message; a provider-side (remote) one is only readable as
		-- that message, so it stays in the prefix and the request is a fresh summary
		local prefix = {}
		for i = 1, start - 1 do
			if not msgs[i].compaction or msgs[i].remote then
				prefix[#prefix + 1] = msgs[i]
			end
		end
		st.local_candidates = (st.local_candidates or 0) + 1
		ctx.bus:emit("compact/stage", { stage = "local", phase = "candidate", messages = cut - 1 })
		local prev_text = prev and not prev.remote and strip_summary(prev.summary or text_of(prev)) or nil
		local instruction = SYSTEM
			.. "\n\n"
			.. (prev_text and ("<previous-summary>\n" .. prev_text .. "\n</previous-summary>\n\n" .. UPDATE) or SUMMARIZE)
			.. focus
		local units = units_of(history)
		local total_units = #units
		local summary, dropped, kind
		-- Matching indices alone is insufficient if a plugin mutates a message in place.
		-- If the input cannot be encoded, skip caching rather than guessing equality.
		local hash_ok, input_hash = pcall(function()
			return mare.crypto.sha256(mare.json.encode({
				prefix = prefix,
				history = history,
				turn = turn,
				instruction = instruction,
			}))
		end)
		local cached = st.local_cache
		local same = hash_ok
			and cached
			and cached.input_hash == input_hash
			and cached.cut == cut
			and cached.start == start
			and cached.history_end == history_end
			and cached.mid_turn == mid_turn
			and cached.instruction == instruction
			and #cached.snapshot == #snapshot
		if same then
			for i, m in ipairs(snapshot) do
				if cached.snapshot[i] ~= m then
					same = false
					break
				end
			end
		end
		if same then
			summary, dropped = cached.summary, cached.dropped
		else
			if #units > 0 or (prev and (prev.remote or not turn)) then
				summary, dropped, kind = summarize(
					prefix,
					units,
					instruction,
					plan.force_portable_remote and { allow_expansion = true } or nil
				)
				if not summary then
					if kind == "nonshrinking" then
						if attempt < 3 and cut <= #msgs then
							st.local_attempt = attempt + 1
							return nil, nil, "retry"
						end
						st.exhausted = true
					end
					return failed(dropped, kind)
				end
			elseif prev_text then
				summary = prev_text
			end
			if turn then
				-- the /compact focus applies to the turn prefix too; defer convergence to the
				-- outer candidate check so a short split-turn slice does not abort a converging run
				local p, e, k = summarize(
					{},
					units_of(turn),
					SYSTEM .. "\n\n" .. TURN_PREFIX .. focus,
					{ allow_expansion = true }
				)
				if not p then
					return failed(e, k)
				end
				summary = (summary and (summary .. "\n\n---\n\n") or "") .. "**Turn context (split turn):**\n\n" .. p
			end
		end
		if not summary then
			return false, "summarization failed"
		end
		-- the history changed while summarizing (e.g. the user stepped in) → discard
		local now = D.messages()
		if #now ~= #snapshot then
			return false, "the conversation changed during compaction; result discarded"
		end
		for i, m in ipairs(snapshot) do
			if now[i] ~= m then
				return false, "the conversation changed during compaction; result discarded"
			end
		end
		if snapshot_hash_ok then
			local now_hash_ok, now_hash = pcall(function()
				return mare.crypto.sha256(mare.json.encode(now))
			end)
			if not now_hash_ok or now_hash ~= snapshot_hash then
				return false, "the conversation changed during compaction; result discarded"
			end
		end
		st.local_cache = {
			input_hash = hash_ok and input_hash or nil,
			cut = cut,
			start = start,
			history_end = history_end,
			mid_turn = mid_turn,
			instruction = instruction,
			snapshot = snapshot,
			summary = summary,
			dropped = dropped,
		}

		local summarized = {}
		for i = start, cut - 1 do
			summarized[#summarized + 1] = msgs[i]
		end
		local files = track_files(summarized, prev and prev.details)
		local full = summary
		-- lossy degradation: say which oldest messages were left out
		local dropped_range
		if dropped and #dropped > 0 then
			local first, last = dropped[1][1], dropped[#dropped][#dropped[#dropped]]
			local n = 0
			for _, u in ipairs(dropped) do
				n = n + #u
			end
			dropped_range = { from = id_of(first), to = id_of(last), messages = n, units = #dropped, of = total_units }
			full = full
				.. ("\n\n_Note: the earliest %d message(s)%s did not fit in the summary request and are not in this summary; the originals are in the session log — read them back with `read` or /tree._"):format(
					n,
					(dropped_range.from and dropped_range.to)
							and (" (entries %s … %s)"):format(dropped_range.from, dropped_range.to)
						or ""
				)
			if mare.ui.connected() then
				ui.notify(
					("Compaction: the earliest %d message(s) didn't fit and were left out of the summary (still in the session log)"):format(
						n
					),
					"warning"
				)
			end
		end
		if #files.read_files > 0 then
			full = full .. "\n\n<read-files>\n" .. table.concat(files.read_files, "\n") .. "\n</read-files>"
		end
		if #files.modified_files > 0 then
			full = full .. "\n\n<modified-files>\n" .. table.concat(files.modified_files, "\n") .. "\n</modified-files>"
		end
		-- include the live todo list, not the model's paraphrase of it
		local TODO_MARK = { pending = "[ ]", in_progress = "[~]", completed = "[x]" }
		local todo = S.svc.todo
		if todo and type(todo.snapshot) == "function" then
			local ok, snap = pcall(todo.snapshot, todo)
			if ok and type(snap) == "table" and type(snap.todos) == "table" and #snap.todos > 0 then
				local lines = {}
				if type(snap.explanation) == "string" and snap.explanation:find("%S") then
					lines[#lines + 1] = snap.explanation
				end
				for _, item in ipairs(snap.todos) do
					if type(item) == "table" and type(item.content) == "string" and item.content:find("%S") then
						lines[#lines + 1] = ("- %s %s"):format(TODO_MARK[item.status] or "[ ]", item.content)
					end
				end
				if #lines > 0 then
					full = full .. "\n\n## Current Todo List\n" .. table.concat(lines, "\n")
				end
			end
		end
		full = full .. recovery_footer(summarized[1] or prev, summarized[#summarized] or prev)

		local kept = {}
		for i = cut, #msgs do
			kept[#kept + 1] = msgs[i]
		end
		local tokens_before = estimate(msgs)
		local wrapped = SUMMARY_PREFIX .. full .. SUMMARY_SUFFIX
		-- The last tier may discard the tail, but never the latest user message: retain
		-- as much of it as fits after the fixed overhead and the actual summary.
		local user_budget = math.floor(retain(cfg.recent_user_tokens) * scale)
		if attempt == 3 then
			user_budget = math.max(
				0,
				math.floor(
					D.threshold() * 0.9
						- D.overhead()
						- estimate(kept)
						- msg_tokens({ content = { { type = "text", text = wrapped } } })
				)
			)
		end
		local preserved = preserve_users(msgs, start, cut, user_budget, attempt == 3 and 1 or nil)
		if attempt == 3 and #preserved == 0 then
			for i = 1, cut - 1 do
				if msgs[i].role == "user" and not msgs[i].compaction and not msgs[i].context_disclosure then
					st.exhausted = true
					return false, "no room to preserve the latest user message below the target"
				end
			end
		end
		local candidate = estimate(preserved)
			+ msg_tokens({ content = { { type = "text", text = wrapped } } })
			+ estimate(kept)
		if
			(not plan.force_portable_remote and candidate >= tokens_before)
			or candidate + D.overhead() > D.threshold() * 0.9
		then
			if
				cut > #msgs
				and candidate >= tokens_before
				and msg_tokens({ content = { { type = "text", text = wrapped } } })
						+ estimate(minimal_users(msgs, start, cut))
					>= tokens_before
			then
				st.exhausted = true
				return false, "summary cannot reduce the context even with minimal retained user text"
			end
			if attempt < 3 then
				st.local_attempt = attempt + 1
				return nil, nil, "retry"
			end
			st.exhausted = true
			return false,
				("summary is not smaller than the replaced context or leaves it above the target after %d attempts (messages %d → %d; overhead %d; threshold %d)"):format(
					st.local_candidates or 0,
					tokens_before,
					candidate,
					D.overhead(),
					D.threshold()
				)
		end
		if D.S.token:is_cancelled() then
			st.stop = "compaction cancelled"
			return false, st.stop
		end
		local details = files
		details.dropped = dropped_range
		local new, cerr = commit(full, kept, tokens_before, details, {
			method = "local",
			range = { summarized[1] or prev, summarized[#summarized] or prev },
			preserved = preserved,
			dropped = dropped_range,
			tokens_after = candidate,
		})
		if not new then
			st.stop = cerr -- a persistence failure stops the compaction
			return false, cerr
		end
		st.summary, st.split, st.mid_turn = full, split, mid_turn or nil
		return true
	end

	--- local: choose a retention tier, request a summary, then validate before commit.
	function stage_impl.local_(st)
		while true do
			local ok, err, action = local_candidate(st)
			if action ~= "retry" and action ~= "restart" then
				return ok, err
			end
		end
	end
end
