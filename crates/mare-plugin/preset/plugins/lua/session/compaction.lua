-- session.compaction: recording compactions and prunes (append_compaction / append_prune);
-- part of the `session` plugin (plugins/session.lua), loaded at the start of its apply.
return function(D)
	local ctx, S, session = D.ctx, D.S, D.session
	local new_id, add = D.store.new_id, D.store.add
	local path_to, context_for = D.history.path_to, D.history.context_for

	--- Every message of the live context is a session entry (so the context can be rebuilt from
	--- the log). Not true for messages a plugin or test put in with agent:set_messages.
	local function all_tracked(msgs)
		for _, m in ipairs(msgs) do
			if not S.ids[m] then
				return false
			end
		end
		return true
	end
	--- Record a compaction; returns the new context.
	--- append_compaction(...) -> new context | nil, err. The compaction is committed in memory
	--- (leaf moved) only after it was durably written.
	--- `extra`: {method, range = {from_msg, to_msg}, tokens_after,
	--- preserved = {user messages kept verbatim}, remote = {provider, model, items},
	--- projections = {[original_message] = replacement_content} (committed with a model-switch summary)}
	function session.append_compaction(_, _, summary, first_kept_msg, tokens_before, details, extra)
		extra = extra or {}
		local live_msgs = ctx.agent:messages()
		local live_tracked = all_tracked(live_msgs)
		local projections = {}
		for msg, content in pairs(extra.projections or {}) do
			if S.ids[msg] then
				projections[S.ids[msg]] = content
			end
		end
		local lib = require("mare_lib")
		local now_ms = lib.to_epoch_ms(mare.task.now())
		local id = new_id()
		local range = extra.range
				and {
					from_entry_id = S.ids[extra.range[1]] or nil,
					to_entry_id = S.ids[extra.range[2]] or nil,
				}
			or nil
		local record = {
			type = "compaction",
			entry_id = id,
			parent_entry_id = S.leaf,
			summary = summary,
			details = details,
			first_kept_entry_id = first_kept_msg and S.ids[first_kept_msg],
			tokens_before = tokens_before,
			tokens_after = extra.tokens_after,
			method = extra.method,
			range = range,
			preserved = extra.preserved,
			remote = extra.remote,
			projections = next(projections) and projections or nil,
			created_at_ms = now_ms,
		}
		local batch = { record }
		if S.sandbox_mode then
			batch[#batch + 1] = {
				type = "custom",
				kind = "sandbox/mode",
				data = S.sandbox_mode,
				provenance = "session-host",
				at_entry_id = id,
				created_at_ms = now_ms,
			}
		end
		if type(S.workspace_dirs) == "table" and #S.workspace_dirs > 0 then
			batch[#batch + 1] = {
				type = "custom",
				kind = "workspace/dirs",
				data = { dirs = S.workspace_dirs },
				provenance = "session-host",
				at_entry_id = id,
				created_at_ms = now_ms,
			}
		end
		local live = {}
		for _, e in ipairs(path_to(S.leaf)) do
			if S.prunes[e.entry_id] then
				live[e.entry_id] = S.prunes[e.entry_id]
			end
		end
		for target, content in pairs(projections) do
			live[target] = content
		end
		if next(live) then
			batch[#batch + 1] = { type = "prune", target_entry_ids = live, created_at_ms = now_ms }
		end
		local m = extra.model or (D.follow.llm() and D.follow.llm():current())
		if m then
			local effort
			if extra.model then
				effort = m.effort
			else
				effort = D.follow.llm():effort()
			end
			batch[#batch + 1] = { type = "model", model = m.ref, effort = effort, at_entry_id = id }
		end
		if S.name then
			batch[#batch + 1] = { type = "name", name = S.name }
		end
		local owner, generation = S.id, S.generation
		local function valid()
			return S.id == owner and S.generation == generation and (not extra.validate or extra.validate())
		end
		local result
		local function finish()
			if live_tracked then
				local new = context_for(id)
				if extra.publish then
					extra.publish(new)
				end
				return new
			end
			-- the live context isn't all session entries: build it from what's there
			local new = context_for(id)
			local start
			for i, m in ipairs(live_msgs) do
				if m == first_kept_msg then
					start = i
					break
				end
			end
			if start then
				local head = {}
				for _, m in ipairs(new) do
					if m.compaction or m.preserved then
						head[#head + 1] = m
					end
				end
				for i = start, #live_msgs do
					local m = live_msgs[i]
					if extra.projections and extra.projections[m] then
						m = table.copy(m)
						m.content, m.pruned = extra.projections[live_msgs[i]], true
					end
					head[#head + 1] = m
				end
				new = head
			end
			if extra.publish then
				extra.publish(new)
			end
			return new
		end
		local ok, err = D.store.add_batch(batch, valid, function()
			for target, content in pairs(projections) do
				S.prunes[target] = content
			end
			S.leaf = id
			result = finish()
		end, {
			persist = true,
			sync = true,
			bind_leaf = function(es, st)
				local leaf = S.leaf
				es[1].parent_entry_id = leaf
				st[1].parent_entry_id = leaf
				return function()
					return S.leaf == leaf
				end
			end,
		})
		if not ok then
			return nil, "session write failed: " .. tostring(err)
		end
		return result
	end
	--- append_prune(map) -> new context | nil, err: replace the content of some messages (by
	--- message table) in the model's view, e.g. oversized old tool outputs cut to head/tail
	--- . Stored history is untouched; the prune entry is
	--- applied whenever the context is rebuilt.
	--- `meta` = {method = "prune"|"offload", tokens_before, tokens_after} (recorded with it)
	function session.append_prune(_, _, map, meta)
		meta = meta or {}
		local targets, n = {}, 0
		for msg, content in pairs(map) do
			local id = S.ids[msg]
			if id then
				targets[id] = content
				n = n + 1
			end
		end
		-- the replacement as seen live (also for messages that aren't session entries)
		local live = ctx.agent:messages()
		local function applied()
			local out = {}
			for _, m in ipairs(live) do
				local c = map[m]
				if c then
					local copy = table.copy(m)
					copy.content, copy.pruned = c, true
					S.ids[copy] = S.ids[m]
					out[#out + 1] = copy
				else
					out[#out + 1] = m
				end
			end
			return out
		end
		if n == 0 then
			if meta.validate and not meta.validate() then
				return nil, "session transaction became stale"
			end
			local new = applied()
			if meta.publish then
				mare.task.publish(meta.publish, new)
			end
			return new
		end
		local batch = {
			{
				type = "prune",
				target_entry_ids = targets,
				method = meta.method,
				tokens_before = meta.tokens_before,
				tokens_after = meta.tokens_after,
				created_at_ms = require("mare_lib").to_epoch_ms(mare.task.now()),
			},
		}
		if meta.model then
			batch[#batch + 1] =
				{ type = "model", model = meta.model.ref, effort = meta.model.effort, at_entry_id = S.leaf }
		end
		local owner, generation, leaf = S.id, S.generation, S.leaf
		local result
		local ok, err = D.store.add_batch(batch, function()
			return S.id == owner
				and S.generation == generation
				and S.leaf == leaf
				and (not meta.validate or meta.validate())
		end, function()
			for id, content in pairs(targets) do
				S.prunes[id] = content
			end
			result = all_tracked(live) and context_for(S.leaf) or applied()
			if meta.publish then
				meta.publish(result)
			end
		end)
		if not ok then
			return nil, "session write failed: " .. tostring(err)
		end
		return result
	end
end
