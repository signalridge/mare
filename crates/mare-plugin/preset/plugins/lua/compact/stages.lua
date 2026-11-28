-- compaction stages and the run loop, for the `compact` service registered by
-- plugins/compact.lua (the entry layer); loaded on the first compaction or summary request
-- . `D` carries the plugin state and the measuring helpers it uses (shared with
-- the entry, never copies); `D.S` holds the mutable state (busy, token, versions).

return function(D)
	local ctx = D.ctx
	local cfg = D.cfg
	local S = D.S
	local tokens = D.tokens
	local msg_tokens = D.msg_tokens
	local estimate = D.estimate
	local measure = D.measure
	local changed = D.changed
	local budgets = D.budgets
	local threshold = D.threshold
	local stages = D.stages
	local SUMMARY_STAGES = D.SUMMARY_STAGES

	local function messages()
		return S.transaction and S.transaction.messages or ctx.agent:messages()
	end
	local function original(m)
		return S.transaction and S.transaction.origins[m] or m
	end
	local function unchanged()
		if S.prepared then
			local current = ctx.agent:messages()
			local expected = S.prepared.history
			if not require("mare_lib").equal_messages(current, expected) then
				return false
			end
		end
		local tx = S.transaction
		if not tx then
			return true
		end
		local now = ctx.agent:messages()
		if #now ~= #tx.original then
			return false
		end
		for i, m in ipairs(tx.original) do
			if now[i] ~= m then
				return false
			end
		end
		return mare.crypto.sha256(mare.json.encode(now)) == tx.hash
	end

	-- structural submodules (lua/compact/stages/), loaded here in dependency order. They get
	-- their own wiring table: D is shared with the entry (and already has D.stages)
	local SD = {
		ctx = ctx,
		messages = messages,
		original = original,
		unchanged = unchanged,
		cfg = cfg,
		S = S,
		tokens = tokens,
		msg_tokens = msg_tokens,
		estimate = estimate,
		changed = changed,
		notify = D.notify,
		budgets = budgets,
		threshold = threshold,
		overhead = D.overhead,
	}
	SD.prompts = require("compact.stages.prompts")
	SD.summarize = require("compact.stages.summarize")(SD)
	SD.cut = require("compact.stages.cut")(SD)
	local cp_len, cp_head, cp_tail = SD.summarize.cp_len, SD.summarize.cp_head, SD.summarize.cp_tail
	local id_of, session_path = SD.summarize.id_of, SD.summarize.session_path
	local units_of, summarize = SD.summarize.units_of, SD.summarize.summarize

	-- ---- stages --------------------------------------------------------------------------

	-- prune / offload and their persistence live in lua/compact/projection.lua, loaded with
	-- the stages (the remote and local stages offload images through it too)
	local projection = require("compact.projection")({
		ctx = ctx,
		S = S,
		messages = messages,
		original = original,
		unchanged = unchanged,
		cfg = cfg,
		estimate = estimate,
		changed = changed,
		notify = D.notify,
		overhead = D.overhead,
		msg_tokens = msg_tokens,
		tokens = tokens,
		cp_len = cp_len,
		cp_head = cp_head,
		cp_tail = cp_tail,
		id_of = id_of,
		session_path = session_path,
		threshold = threshold,
		usage_current = function()
			return S.usage_version == S.version
		end,
	})
	local persist_projection, offload_plan = projection.persist_projection, projection.offload_plan
	local stage_impl = { prune = projection.prune, offload = projection.offload }
	-- the remote and local stages need the projection helpers: loaded once it exists
	SD.persist_projection, SD.offload_plan, SD.stage_impl = persist_projection, offload_plan, stage_impl
	SD.commit = require("compact.stages.commit")(SD)
	require("compact.stages.remote")(SD)
	require("compact.stages.local_summary")(SD)

	--- run(instructions?, opts?) → {status, tokens_before, tokens_after, summary?, dropped?} | nil, reason, kind
	--- opts: {cancel (token or fun(): Token?), reason = "auto"|"overflow"|"manual" (default manual)}
	--- auto stops as soon as the context is below the threshold; manual / overflow continue
	--- until a summary stage succeeds (an overflow means the estimate can't be trusted). A
	--- cancel, a policy refusal or a persistence failure stops at once.
	local function run(instructions, opts)
		opts = opts or {}
		if S.busy then
			return nil, "already compacting"
		end
		local cancel = opts.cancel
		if type(cancel) == "function" then
			cancel = cancel()
		end
		local reason = opts.reason or "manual"
		local units
		if opts.summarize_only then
			units = units_of(SD.summarize.repair_tool_order(opts.messages or {}))
			if #units == 0 then
				return nil, "nothing to summarize"
			end
		else
			if reason == "model_switch" and not S.transaction then
				return nil, "model_switch requires an active model switch"
			end
			if reason ~= "auto" and reason ~= "overflow" and ctx.agent and ctx.agent:activity().busy then
				return nil, "agent is busy"
			end
		end
		local my_token = cancel and cancel:child() or mare.task.token()
		local released = false
		local function release_admission()
			if released then
				return
			end
			released = true
			my_token:cancel()
			if S.token == my_token then
				S.token = nil
				S.busy = false
				S.released()
			end
		end
		S.busy = true
		S.token = my_token
		local busy_guard <close> = setmetatable({}, {
			__close = release_admission,
		})
		-- summarize given messages without touching the context (/tree branch summaries): the same
		-- budget and oldest-first protocol-unit dropping as the local stage → { status =
		-- "summarized", tokens_before, tokens_after, summary, dropped } | nil, err
		if opts.summarize_only then
			local input_tokens = estimate(opts.messages)
			local ok, summary, dropped, kind = pcall(summarize, {}, units, instructions or "")
			local cancelled = my_token:is_cancelled()
			release_admission()
			if cancelled then
				return nil, "compaction cancelled", "cancelled"
			end
			if not ok then
				return nil, tostring(summary)
			end
			if not summary then
				return nil, dropped, kind
			end
			local summary_tokens = tokens(summary)
			if input_tokens > 0 and summary_tokens >= input_tokens then
				return nil, "summary did not shrink the input"
			end
			return {
				status = "summarized",
				tokens_before = input_tokens,
				tokens_after = summary_tokens,
				summary = summary,
				dropped = #(dropped or {}),
			}
		end
		local model = (S.prepared and S.prepared.model) or S.target_model or ctx.llm:current()
		local limit, cerr = threshold(model)
		if not limit then
			return nil, cerr
		end
		local before = measure()
		local compact_id = mare.crypto.random_hex(8)
		local item_turn_id = S.active_turn_id
		local handled = ctx.bus:bail("compact/before", { reason = reason, tokens = before, threshold = limit })
		if not my_token:is_cancelled() then
			if handled == false then
				return nil, "compaction cancelled by a plugin"
			elseif handled ~= nil then
				return { status = "unchanged", tokens_before = before, tokens_after = before }
			end
		end
		ctx.bus:emit("compact/started", { compact_id = compact_id, reason = reason, messages = #messages() })
		if item_turn_id then
			ctx.bus:emit("item/started", {
				turn_id = item_turn_id,
				item_id = compact_id,
				item = { type = "compaction", reason = reason },
			})
		end
		local st = {
			reason = reason,
			instructions = instructions,
			over = before > limit,
			force_prune = reason == "overflow",
		}
		local version_before = S.version
		local local_before = estimate(messages())
		local did, summarized, errs, skips = false, false, {}, {}
		local NO_OP_REASONS = {
			["no old tool output to prune"] = true,
			["pruning would save too little"] = true,
			["no older images to offload"] = true,
			["disabled"] = true,
			["skipped: a focus needs the local summary"] = true,
			["nothing to compact"] = true,
			["model switches require a text summary"] = true,
			["unsupported"] = true,
		}
		local ok, perr = pcall(function()
			if my_token:is_cancelled() then
				errs[#errs + 1] = "cancelled"
				return
			end
			for _, name in ipairs(cfg.stages) do
				local fn = stages[name]
				if fn then
					local tb = measure()
					ctx.bus:emit("compact/stage", { stage = name, phase = "before", tokens = tb })
					if my_token:is_cancelled() then
						errs[#errs + 1] = "cancelled"
						break
					end
					local good, why = fn(st)
					local ta = measure()
					ctx.bus:emit("compact/stage", {
						stage = name,
						phase = "after",
						changed = good == true,
						reason = why,
						tokens_before = tb,
						tokens_after = ta,
					})
					if good then
						did = true
						if SUMMARY_STAGES[name] then
							summarized = true
						end
					elseif why then
						local note = name .. ": " .. tostring(why)
						if NO_OP_REASONS[tostring(why)] then
							skips[#skips + 1] = note
						else
							errs[#errs + 1] = note
						end
					end
					if my_token:is_cancelled() then
						errs[#errs + 1] = "cancelled"
						break
					end
					if st.stop then
						break
					end
					if (reason == "auto" and ta <= limit) or (reason == "model_switch" and ta < limit) then
						break
					end
					if reason ~= "auto" and summarized then
						break
					end
				end
			end
		end)
		-- Esc / Ctrl+C: the session stays as it was and nothing is reported
		-- as an error
		local cancelled = my_token:is_cancelled() and not (S.transaction and S.transaction.committed)
		if not ok then
			if mare.task.session_error() then
				release_admission()
				error(tostring(perr), 0)
			end
			-- a stage raising is unexpected: for a model switch it goes on as it is (the caller
			-- refuses only for reported failures; outside callers see `internal`)
			if reason == "model_switch" then
				error(perr, 0)
			end
			errs[#errs + 1] = tostring(perr)
		end
		local after = measure()
		local local_after = estimate(messages())
		local progressed = (did or S.version > version_before) and local_after < local_before
		local success
		if reason == "model_switch" then
			success = not cancelled and ok and after < limit
			if success and not summarized then
				if not unchanged() then
					success = false
					errs[#errs + 1] = "the conversation changed during compaction; result discarded"
				else
					local tx = S.transaction
					tx.commit = function(publish)
						local prepared_overhead = D.overhead(tx.target_model)
						local function apply(new)
							tx.committed, tx.messages = true, new
							ctx.agent:set_messages(new, { notify = false })
							publish()
							changed(new, prepared_overhead, false)
						end
						if S.svc.session then
							local new, err = S.svc.session:append_prune(tx.projections, {
								method = "model_switch",
								model = tx.model,
								tokens_before = local_before,
								tokens_after = local_after,
								validate = unchanged,
								publish = apply,
							})
							if not new then
								error(err, 0)
							end
						else
							mare.task.publish(function()
								assert(unchanged(), "the conversation changed during compaction; result discarded")
								apply(tx.messages)
							end)
						end
						D.notify(tx.messages)
					end
				end
			end
		elseif reason == "auto" then
			-- below the threshold, or a summary was written (what's left is the kept tail)
			success = after <= limit or summarized
		elseif reason == "overflow" then
			-- overflow retries when a summary succeeded or persisted projections shrank the context;
			-- without progress (or on cancel / stop) it must not loop; a stage that raised is a
			-- failure even after earlier persisted progress — its error is reported, not retried over
			success = ok and not cancelled and not st.stop and (summarized or progressed)
		else
			success = summarized
		end
		local err = not success
				and (#errs > 0 and table.concat(errs, "; ") or (#skips > 0 and table.concat(skips, "; ") or "nothing to compact"))
			or nil
		local stalled_info = nil
		if st.exhausted and not success and reason == "auto" and not cancelled then
			S.stalled = true
			local oh = D.overhead(model)
			local summary = 0
			for _, m in ipairs(ctx.agent:messages()) do
				---@cast m table
				if m.compaction then
					summary = summary + msg_tokens(m)
				end
			end
			local kept = math.max(0, after - oh - summary)
			local attempts = st.local_candidates or 0
			local text = (
				"compaction can't get the context below the compaction threshold: after %d attempts "
				.. "it is still %d tokens (threshold %d, window %d; system prompt and tools %d, summary %d, "
				.. "kept messages %d) — start a new session (/new), switch to a model with a larger context "
				.. "window, or lower compact.keep_recent_tokens / recent_user_tokens (or raise the threshold)"
			):format(attempts, after, limit, model.context_window, oh, summary, kept)
			stalled_info = {
				tokens = after,
				threshold = limit,
				window = model.context_window,
				compactions = attempts,
				overhead = oh,
				summary_tokens = summary,
				kept = kept,
				message = text,
			}
			ctx.agent:cancel({ mode = "halt", reason = text, clear_queue = false })
		end
		release_admission()
		local changed_any = did or S.version > version_before
		local compact_status = (stalled_info and "stalled")
			or (cancelled and "cancelled")
			or (success and "completed")
			or "failed"
		local ev_err = require("mare_lib").structured_error(err, (cancelled and "cancelled") or "compact.not_compacted")
		local completed_payload = {
			compact_id = compact_id,
			reason = reason,
			status = compact_status,
			stalled = stalled_info ~= nil or nil,
			summary = st.summary,
			tokens_before = before,
			tokens_after = after,
			pruned_count = st.pruned_count,
			offloaded_count = st.offloaded,
			split = st.split,
			mid_turn = st.mid_turn,
			error = ev_err,
			changed = changed_any,
			cancelled = cancelled or nil,
		}
		if stalled_info then
			for k, v in pairs(stalled_info) do
				completed_payload[k] = v
			end
		end
		ctx.bus:emit("compact/completed", completed_payload)
		if item_turn_id then
			local item_status = (cancelled and "cancelled") or (success and "completed") or "failed"
			ctx.bus:emit("item/completed", {
				turn_id = item_turn_id,
				item_id = compact_id,
				status = item_status,
				item = {
					type = "compaction",
					reason = reason,
					tokens_before = before,
					tokens_after = after,
					pruned_count = st.pruned_count,
					offloaded_count = st.offloaded,
					changed = changed_any,
					summary = st.summary,
					stalled = stalled_info ~= nil or nil,
					error = ev_err,
					cancelled = cancelled or nil,
				},
			})
		end
		if success then
			S.warned_near = false
		end
		if not success then
			if #errs == 0 and not cancelled and not st.exhausted and not st.stop and reason ~= "model_switch" then
				return { status = "nothing_to_compact", tokens_before = before, tokens_after = after }
			end
			return nil, err, cancelled and "cancelled" or nil
		end
		return {
			status = (did or summarized or S.version > version_before) and "compacted" or "unchanged",
			tokens_before = before,
			tokens_after = after,
		}
	end

	return {
		prune = stage_impl.prune,
		offload = stage_impl.offload,
		remote = stage_impl.remote,
		local_ = stage_impl.local_,
		run = run,
	}
end
