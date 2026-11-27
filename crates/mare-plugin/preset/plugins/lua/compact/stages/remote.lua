-- compact.stages.remote: the provider-side (remote) stage, stored into the shared stage table;
-- part of the lazy `compact.stages` module (lua/compact/stages.lua),
-- loaded at the start of its factory, i.e. on the first compaction.
return function(D)
	local ctx, cfg, S = D.ctx, D.cfg, D.S
	local tokens, estimate, threshold = D.tokens, D.estimate, D.threshold
	local persist_projection, offload_plan, stage_impl = D.persist_projection, D.offload_plan, D.stage_impl
	local REMOTE_PLACEHOLDER = D.prompts.REMOTE_PLACEHOLDER
	local track_files = D.cut.track_files
	local commit, recovery_footer = D.commit.commit, D.commit.recovery_footer
	local repair_tool_order = D.summarize and D.summarize.repair_tool_order or function(msgs)
		return msgs
	end

	--- remote: provider-side compaction (OpenAI /responses/compact). Replaces the whole
	--- context with the provider's compacted history. Falls back to local only on errors that
	--- allow it; a cancel or a policy refusal stops the compaction.
	function stage_impl.remote(st)
		if not cfg.remote or not ctx.llm.compact_remote then
			return false, "disabled"
		end
		-- the provider's compaction takes no instructions: /compact <focus> goes to the local
		-- summary, which does
		if st.instructions and st.instructions:find("%S") then
			return false, "skipped: a focus needs the local summary"
		end
		local msgs = D.messages()
		if #msgs < 2 then
			return false, "nothing to compact"
		end
		local model = ctx.llm:current()
		-- Opaque state belongs to its source model; a switch needs a portable text summary.
		if S.target_model or (S.prepared and (not model or S.prepared.model.ref ~= model.ref)) then
			return false, "model switches require a text summary"
		end
		-- only a provider with a compaction endpoint gets this far: offloading first would
		-- replace even the latest turn's images for a request that is never sent
		if ctx.llm.supports_remote_compact and not ctx.llm:supports_remote_compact(model) then
			return false, "unsupported"
		end
		-- Remote compaction replaces the whole context, so every input image needs a durable
		-- attachment reference before the provider can summarize it.
		local map, count, failed = offload_plan(msgs, 0)
		if failed > 0 then
			st.stop = ("failed to offload %d image(s) before summarizing"):format(failed)
			return false, st.stop
		end
		if count > 0 then
			local new, perr = persist_projection(map, count, "offload", msgs)
			if not new then
				st.stop = perr or "failed to persist image offload"
				return false, st.stop
			end
			msgs = new
		end
		local snapshot = table.copy(msgs)
		-- A shallow snapshot alone cannot detect plugins editing message content in place.
		-- Fail closed if we cannot fingerprint the selected input before yielding.
		local hash_ok, snapshot_hash = pcall(function()
			return mare.crypto.sha256(mare.json.encode(msgs))
		end)
		if not hash_ok then
			st.stop = "cannot verify the conversation before compaction; result discarded"
			return false, st.stop
		end
		local system, tools
		if S.prepared then
			system, tools = S.prepared.system or "", S.prepared.tools or {}
		else
			system = S.svc.context and S.svc.context:system_prompt(model) or ""
			tools = S.svc.tools and S.svc.tools:definitions(model) or {}
		end
		local res, err, kind = ctx.llm:compact_remote({
			model = model,
			system = system,
			tools = tools,
			messages = repair_tool_order(msgs),
			session_id = S.svc.session and S.svc.session:info().session_id or nil,
			cancel = S.token,
		})
		local REMOTE_FALLBACK_KINDS = {
			unsupported = true,
			server = true,
			transport = true,
			invalid = true,
		}
		if not res then
			if S.token:is_cancelled() or not REMOTE_FALLBACK_KINDS[kind] then
				st.stop = err
			end
			return false, err
		end
		if S.token:is_cancelled() then
			st.stop = "compaction cancelled"
			return false, st.stop
		end
		local now = D.messages()
		if #now ~= #snapshot then
			return false, "the conversation changed during compaction; result discarded"
		end
		for i, m in ipairs(snapshot) do
			if now[i] ~= m then
				return false, "the conversation changed during compaction; result discarded"
			end
		end
		local now_hash_ok, now_hash = pcall(function()
			return mare.crypto.sha256(mare.json.encode(now))
		end)
		if not now_hash_ok or now_hash ~= snapshot_hash then
			return false, "the conversation changed during compaction; result discarded"
		end
		if type(res.items) ~= "table" or #res.items == 0 then
			return false, "the provider's compacted history has no items"
		end
		local has_compaction = false
		for _, item in ipairs(res.items) do
			if type(item) == "table" and item.type == "compaction" then
				has_compaction = true
				break
			end
		end
		if not has_compaction then
			return false, "the provider's compacted history is missing a compaction item"
		end
		local size = tokens(res.items)
		local tokens_before = estimate(msgs)
		if size >= tokens_before or size + D.overhead(model) > (threshold(model) or math.huge) * 0.9 then
			return false, "the provider's compacted history is not smaller or is still too large"
		end
		local prev_details
		for _, m in ipairs(msgs) do
			if m.compaction then
				prev_details = m.details
			elseif not m.preserved then
				break
			end
		end
		local files = track_files(msgs, prev_details)
		local text = REMOTE_PLACEHOLDER .. recovery_footer(msgs[1], msgs[#msgs])
		local new, cerr = commit(text, {}, tokens_before, files, {
			method = "remote",
			range = { msgs[1], msgs[#msgs] },
			tokens_after = size,
			remote = { provider_id = res.provider_id, model_id = res.model_id, items = res.items },
		})
		if not new then
			st.stop = cerr -- a persistence failure stops the compaction
			return false, cerr
		end
		st.summary = text
		return true
	end
end
