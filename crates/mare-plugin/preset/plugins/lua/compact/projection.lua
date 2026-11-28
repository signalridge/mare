-- Persisted projection stages for compaction. Created on first use so the entry keeps
-- request-time registrations available without initializing image and prune implementations.
return function(deps)
	local ctx, cfg = deps.ctx, deps.cfg
	local S = deps.S
	local estimate, changed, msg_tokens, tokens = deps.estimate, deps.changed, deps.msg_tokens, deps.tokens
	local cp_len, cp_head, cp_tail = deps.cp_len, deps.cp_head, deps.cp_tail
	local id_of, session_path, usage_current = deps.id_of, deps.session_path, deps.usage_current

	local function persist_projection(map, n, method, msgs)
		local before = estimate(msgs)
		local tx = deps.S and deps.S.transaction
		if tx then
			local new = {}
			for _, m in ipairs(msgs) do
				if map[m] then
					local copy = table.copy(m)
					copy.content, copy.pruned = map[m], true
					local origin = tx.origins[m] or m
					tx.origins[copy], tx.projections[origin] = origin, map[m]
					new[#new + 1] = copy
				else
					new[#new + 1] = m
				end
			end
			tx.messages = new
			return new, before, estimate(new)
		end
		local prepared_overhead = deps.overhead()
		local ag = ctx.agent
		local snapshot_hash = mare.crypto.sha256(mare.json.encode(ag:messages()))
		local function unchanged()
			return deps.unchanged() and mare.crypto.sha256(mare.json.encode(ag:messages())) == snapshot_hash
		end
		local function publish(new)
			ctx.agent:set_messages(new, { notify = false })
			changed(new, prepared_overhead, false)
		end
		local projected = {}
		for _, m in ipairs(msgs) do
			if map[m] then
				local c = table.copy(m)
				c.content, c.pruned = map[m], true
				projected[#projected + 1] = c
			else
				projected[#projected + 1] = m
			end
		end
		local after = estimate(projected)
		local new
		if S.svc.session then
			local err
			new, err = S.svc.session:append_prune(map, {
				method = method,
				tokens_before = before,
				tokens_after = after,
				validate = unchanged,
				publish = publish,
			})
			if not new then
				return nil, err
			end
		else
			new = projected
		end
		if not S.svc.session then
			mare.task.publish(function()
				assert(unchanged(), "the conversation changed during compaction; result discarded")
				publish(new)
			end)
		end
		deps.notify(new)
		return new, before, estimate(new)
	end

	local stage_impl = {}
	local PRUNE_MARK = "tool result middle pruned"

	--- prune: old tool outputs above the size limit → head + tail + where the original is.
	--- The most recent `prune_protect_tokens` stay; only done when it saves enough (it changes
	--- the request prefix once). Messages after the prompt-cache boundary go first;
	--- the cached prefix is touched only when that isn't enough and the context is over the
	--- threshold anyway (a summary would break the cache entirely).
	function stage_impl.prune(st)
		local msgs = deps.messages()
		local limit = deps.threshold and deps.threshold() or nil
		local target_budget = limit and math.max(0, limit - deps.overhead()) or nil
		local base_protect = cfg.prune_protect_tokens
		local base_min_saving = cfg.prune_min_saving_tokens
		local function find_protect_from(protect)
			local acc, idx = 0, #msgs + 1
			for i = #msgs, 1, -1 do
				acc = acc + msg_tokens(msgs[i])
				if acc > protect then
					break
				end
				idx = i
			end
			return idx
		end
		local cached = 0
		if S.svc.usage and usage_current() then
			cached = (S.svc.usage:totals() or {}).last_cache_read or 0
		end
		local function plan(skip_cached, from)
			local map, n, saved, offset = {}, 0, 0, 0
			for i = 1, from - 1 do
				local m = msgs[i]
				offset = offset + msg_tokens(m)
				-- `m.pruned` only says the content was projected (prune or offload): a result whose
				-- images were offloaded still has prunable text; text already pruned carries the
				-- marker
				if not (skip_cached and offset <= cached) and m.role == "tool_result" then
					local content, gain = nil, 0
					-- block by block: only text shrinks; images and other blocks stay as they are
					--
					for bi, b in ipairs(m.content or {}) do
						local t = b.type == "text" and b.text or nil
						local len = t and cp_len(t) or 0
						if
							type(t) == "string"
							and len > cfg.prune_tool_result_chars
							and not t:find(PRUNE_MARK, 1, true)
						then
							local id = id_of(m)
							local where = id
									and ("session log %s, entry %s"):format(session_path() or "(this session)", id)
								or "the session log"
							-- prune marker, plus where the original is
							local note = ("\n\n[... %s (%d characters; the full output is in %s) ...]\n\n"):format(
								PRUNE_MARK,
								len - cfg.prune_head_chars - cfg.prune_tail_chars,
								where
							)
							local text = cp_head(t, cfg.prune_head_chars) .. note .. cp_tail(t, cfg.prune_tail_chars)
							content = content or table.copy(m.content)
							content[bi] = { type = "text", text = text }
							gain = gain + tokens(t) - tokens(text)
						end
					end
					if content then
						map[m] = content
						n = n + 1
						saved = saved + gain
					end
				end
			end
			return map, n, saved
		end
		local function plan_tier(protect, floor, allow_break_cache)
			local from = find_protect_from(protect)
			local map, n, saved = plan(cached > 0, from)
			if saved < floor and cached > 0 and allow_break_cache then
				map, n, saved = plan(false, from)
			end
			return map, n, saved, from
		end
		-- Try the configured protection tail and saving floor first; only fall back to
		-- threshold-scaled tiers if the configured strategy makes no or insufficient progress.
		local min_saving = base_min_saving
		local map, n, saved, protect_from = plan_tier(base_protect, base_min_saving, st.over or st.force_prune)
		local needs_fallback = n == 0 or (saved < min_saving and not st.force_prune)
		if needs_fallback and (st.over or st.force_prune) and target_budget then
			local fallback_protect = math.min(base_protect, math.floor(target_budget * 0.5))
			local fallback_min_saving = math.min(base_min_saving, math.max(1, math.floor(target_budget * 0.25)))
			if fallback_min_saving < base_min_saving then
				min_saving = fallback_min_saving
				map, n, saved, protect_from = plan_tier(base_protect, fallback_min_saving, true)
				needs_fallback = n == 0 or (saved < min_saving and not st.force_prune)
			end
			if needs_fallback and fallback_protect < base_protect then
				min_saving = fallback_min_saving
				map, n, saved, protect_from = plan_tier(fallback_protect, fallback_min_saving, true)
			end
		end
		if n == 0 and st.force_prune and protect_from <= #msgs then
			local relaxed = find_protect_from(math.floor(estimate(msgs) * 0.25))
			if relaxed > protect_from then
				map, n, saved = plan(false, relaxed)
			end
			if n == 0 then
				map, n, saved = plan(false, #msgs + 1)
			end
		end
		if n == 0 or (saved < min_saving and not st.force_prune) then
			return false, n == 0 and "no old tool output to prune" or "pruning would save too little"
		end
		local new, before, after = persist_projection(map, n, "prune", msgs)
		if not new then
			st.stop = before or "failed to persist pruning"
			return false, st.stop
		end
		st.pruned_count = (st.pruned_count or 0) + n
		return true
	end

	local function attachment_dir()
		local p = session_path()
		if p then
			return require("mare_lib").attachments_dir(p)
		end
		return mare.paths.state .. "/attachments/" .. (S.svc.session and S.svc.session:info().session_id or "unsaved")
	end
	local EXT = { ["image/png"] = "png", ["image/jpeg"] = "jpg", ["image/gif"] = "gif", ["image/webp"] = "webp" }

	--- write one image block to a file; returns the path or nil
	local function write_image(b, stem)
		local ok, bytes = pcall(mare.crypto.base64_decode, b.data or "")
		if not ok or not bytes or bytes == "" then
			return nil
		end
		local dir = attachment_dir()
		-- Unsaved messages have position-based stems that can be reused by later contexts.
		-- Include the content digest so an older reference never points at another image, and
		-- rewrite atomically even if the path exists (it might have been modified externally).
		local path = ("%s/%s-%s.%s"):format(dir, stem, mare.crypto.sha256(bytes), EXT[b.mime or ""] or "bin")
		if mare.fs.write(path, bytes, { atomic = true, mode = 384 }) then
			return path
		end
		return nil
	end

	--- images of `msgs` (except the most recent `keep`, or all when keep = 0) → file references.
	--- Returns the replacement map and how many images were offloaded / couldn't be.
	local function offload_plan(msgs, keep, from, to)
		local seen, map, n, failed = 0, {}, 0, 0
		for i = #msgs, 1, -1 do
			local m = msgs[i]
			-- only input messages (user / tool results); assistant output is never touched
			local inside = i >= (from or 1) and i <= (to or #msgs) and m.role ~= "assistant"
			local content, touched = nil, false
			for bi = #(m.content or {}), 1, -1 do
				local b = m.content[bi]
				if b.type == "image" then
					seen = seen + 1
					if seen > keep and inside then
						local stem = (id_of(m) or ("m" .. i)) .. "-" .. bi
						local path = write_image(b, stem)
						if path then
							content = content or table.copy(m.content)
							content[bi] = {
								type = "text",
								-- recovery wording with the file it was written to
								text = ("[Attachment: %s (read-only image offloaded to save context; re-read the file to view it)]"):format(
									path
								),
							}
							touched = true
							n = n + 1
						else
							failed = failed + 1
						end
					end
				end
			end
			if touched then
				map[m] = content
			end
		end
		return map, n, failed
	end

	--- offload: images older than the most recent `offload_keep_images` → attachment files
	--- (`st.keep_images` overrides it: a provider's image-limit error offloads more)
	function stage_impl.offload(st)
		local msgs = deps.messages()
		local map, n = offload_plan(msgs, st.keep_images or cfg.offload_keep_images)
		if n == 0 then
			return false, "no older images to offload"
		end
		local new, before, after = persist_projection(map, n, "offload", msgs)
		if not new then
			st.stop = before or "failed to persist image offload"
			return false, st.stop
		end
		st.offloaded = n
		return true
	end

	return {
		prune = stage_impl.prune,
		offload = stage_impl.offload,
		offload_plan = offload_plan,
		persist_projection = persist_projection,
	}
end
