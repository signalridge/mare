-- Key binding API: contexts, binding stacks, setup.keys shorthand and /reload snapshots.
-- Installed by the core before init.lua; default bindings belong to the keymap-default plugin.

local mare, core = ...
local km_host = core and core.keymap
local cfg_core = core and core.config
local ui = mare.ui

-- Config Lua can yield; only registrations made by its own coroutine belong to its reload.
local function config_state()
	return cfg_core and cfg_core.state or nil
end

local function in_config()
	local st = config_state()
	return st ~= nil and st.in_config == true and st.config_coroutine == coroutine.running()
end

local function config_stage()
	local st = config_state()
	return st and st.stage or nil
end

---------------------------------------------------------------------------
-- keymaps (vim.keymap-style). Actions are core action names ("transcript", "interrupt",
-- "follow-up", "complete", "newline", "quit-if-empty", "quit", "redraw",
-- "suspend", "steer", "history-search", "external-editor", "paste-image", "select-mode",
-- "view.*") or Lua functions, or false to mask lower bindings. Contexts include "global",
-- "text", "editor", "transcript", their core subcontexts, and plugin subcontexts declared with
-- mare.keymap.context; a binding's `when` names predicates the core evaluates (no Lua per key).
---------------------------------------------------------------------------
mare.keymap = {}

local maps = {}
local fns = {}
local refs = {}
local fn_seq = 0
local bind_seq = 0
local actions = {}
local settled = false
local reload_depth = 0
local reload_snap = nil
local pre_entries = nil
local pre_sigs = nil

local core_action_descs = nil
local core_action_declines = nil
local function load_core_actions()
	core_action_descs = {}
	core_action_declines = {}
	for _, a in ipairs(km_host.core_actions()) do
		core_action_descs[a.name] = a.desc
		if a.declines then
			core_action_declines[a.name] = true
		end
	end
end
local function core_desc(name)
	if not core_action_descs then
		load_core_actions()
	end
	return core_action_descs[name]
end
local function core_declines(name)
	if not core_action_declines then
		load_core_actions()
	end
	return core_action_declines[name] == true
end
local function is_core_action(name)
	if not core_action_descs then
		load_core_actions()
	end
	return core_action_descs[name] ~= nil
end

local function layer_name(rank)
	if rank == 4 then
		return "project"
	elseif rank == 3 then
		return "user"
	elseif rank == 2 then
		return "plugin"
	end
	return "default"
end

local function source_info()
	local layer = mare.config_origin().layer
	if layer == "project" then
		return 4, "project", "project"
	elseif layer == "user" then
		return 3, "user", "user"
	end
	if km_host and km_host.current_owner then
		local plugin, origin = km_host.current_owner()
		if plugin ~= nil then
			if origin == "builtin" then
				return 1, "plugin:" .. plugin, "default"
			end
			return 2, "plugin:" .. plugin, "plugin"
		end
	end
	return 2, "runtime", "plugin"
end

-- Public opts.owner / opts.layer attribution for delegating services (e.g. ui.set_keymap, ui:action).
-- Layer is capped to "default" (rank 1) or "plugin" (rank 2); user/project ranks only come from config.
local function resolve_source(fn_name, opts, level)
	local rank, owner, layer = source_info()
	local explicit = false
	if type(opts) == "table" then
		if opts.layer ~= nil then
			if opts.layer == "default" then
				rank = 1
				layer = "default"
			elseif opts.layer == "plugin" then
				rank = 2
				layer = "plugin"
			else
				error(('mare.keymap.%s: opts.layer must be "default" or "plugin"'):format(fn_name), level + 1)
			end
			explicit = true
		end
		if opts.owner ~= nil then
			if type(opts.owner) ~= "string" or opts.owner == "" then
				error(("mare.keymap.%s: opts.owner must be a non-empty string"):format(fn_name), level + 1)
			end
			owner = opts.owner
			explicit = true
		end
	end
	return rank, owner, layer, explicit
end

local function when_overlaps(a_all, a_none, b_all, b_none)
	return (a_all & b_none) == 0 and (a_none & b_all) == 0
end

local function state_mask(all, none)
	local mask = 0
	for s = 0, 15 do
		if (s & all) == all and (s & none) == 0 then
			mask = mask | (1 << s)
		end
	end
	return mask
end

local function err_level(level)
	return level == 0 and 0 or (level + 1)
end

local function parse_priority(opts, fn_name, level)
	local p = opts and opts.priority
	if p == nil then
		return 0
	end
	if type(p) ~= "number" or p ~= p or p == math.huge or p == -math.huge then
		error(("mare.keymap.%s: priority must be a finite number"):format(fn_name), err_level(level))
	end
	return p
end

-- Restore old entries at their original (rank, priority, seq) position as well as inserting new ones.
-- At equal (rank, priority), a mask (`action == false`) deterministically outranks non-masks.
local function insert_entry(stack, entry)
	local i = #stack + 1
	local ep = entry.priority or 0
	local em = entry.action == false
	while i > 1 do
		local previous = stack[i - 1]
		local pp = previous.priority or 0
		local pm = previous.action == false
		if
			previous.rank < entry.rank
			or (
				previous.rank == entry.rank
				and ((not pm and em) or (pm == em and (pp < ep or (pp == ep and previous.seq < entry.seq))))
			)
		then
			break
		end
		i = i - 1
	end
	table.insert(stack, i, entry)
end

local function retain(id)
	if fns[id] then
		refs[id] = (refs[id] or 0) + 1
	end
end

local function release(id)
	local count = refs[id]
	if count then
		count = count - 1
		refs[id] = count > 0 and count or nil
		if count == 0 then
			fns[id] = nil
		end
	end
end

local function all_contexts()
	return km_host.contexts()
end

local function build_binding_list(st, ctx)
	local list = {}
	for _, e in ipairs(st or {}) do
		if e.context == ctx then
			-- a Lua action or function can decline, so it doesn't hide the bindings below it
			local unbound = e.action == false
			local lua = not unbound and (fns[e.action] ~= nil or actions[e.action] ~= nil or core_declines(e.action))
			list[#list + 1] = {
				action = unbound and "" or e.action,
				unbound = unbound,
				rank = e.rank,
				priority = e.priority or 0,
				seq = e.seq,
				all = e._all,
				none = e._none,
				lua = lua,
			}
		end
	end
	return list
end

local function flush_updates(updates)
	local n = #updates
	if n == 0 then
		return
	end
	if n == 1 then
		local u = updates[1]
		km_host.set_bindings(u[1], u[2], u[3])
		return
	end
	km_host.set_bindings_batch(updates)
end

local function rebind(key, only_contexts)
	local st = maps[key]
	if st and #st == 0 then
		maps[key] = nil
		st = nil
	end
	if only_contexts then
		local seen = {}
		for _, ctx in ipairs(only_contexts) do
			if not seen[ctx] then
				seen[ctx] = true
				km_host.set_bindings(ctx, key, build_binding_list(st, ctx))
			end
		end
		return
	end
	for _, ctx in ipairs(all_contexts()) do
		km_host.set_bindings(ctx, key, build_binding_list(st, ctx))
	end
end

local function normalize_seq(key)
	return ui.normalize_key(key)
end

-- the condition bits and validity of a binding in `context`
local function check(fn, context, key, when, action, level)
	local act = type(action) == "string" and action or nil
	local all, none, err = km_host.check(tostring(context), key, when, act)
	if not all then
		error(("mare.keymap.%s: %s"):format(fn, err), level + 1)
	end
	return all, none
end

-- the newest registration of a named action
local function named_action(name)
	local st = actions[name]
	return st and st[#st]
end

local function check_action_context(fn_name, action, context, level)
	if type(action) ~= "string" then
		return
	end
	if is_core_action(action) then
		return
	end
	local a = named_action(action)
	if a then
		if type(a.contexts) == "table" and #a.contexts > 0 and km_host and km_host.context_info then
			local info = km_host.context_info(tostring(context))
			local chain = info and info.chain or {}
			local ok = false
			for _, allowed in ipairs(a.contexts) do
				if table.find(chain, allowed) then
					ok = true
					break
				end
			end
			if not ok then
				error(
					("mare.keymap.%s: action %q is not available in context %q"):format(
						fn_name,
						action,
						tostring(context)
					),
					err_level(level)
				)
			end
		end
	elseif settled and not in_config() then
		error(("mare.keymap.%s: unknown action %q"):format(fn_name, action), err_level(level))
	end
end

local function sorted_map_keys(m)
	local keys = {}
	for k in pairs(m) do
		keys[#keys + 1] = k
	end
	table.sort(keys)
	return keys
end

local function is_pre_existing(key, e)
	if not pre_sigs then
		return false
	end
	if pre_entries and pre_entries[e] then
		return true
	end
	local sig = ("%s\0%s\0%s\0%d\0%s\0%d\0%d"):format(
		key,
		e.context,
		e.owner or "",
		e.rank or 0,
		tostring(e.priority or 0),
		e._all or 0,
		e._none or 0
	)
	return pre_sigs[sig] == true
end

local function find_ambiguity_conflicts(m, honor_pre)
	local bad, diags = {}, {}
	for _, key in ipairs(sorted_map_keys(m)) do
		local st = m[key] or {}
		for i = 1, #st do
			local a = st[i]
			if a.action ~= false then
				for j = i + 1, #st do
					local b = st[j]
					if
						b.action ~= false
						and a.context == b.context
						and a.rank == b.rank
						and (a.priority or 0) == (b.priority or 0)
						and a.owner ~= b.owner
						and when_overlaps(a._all, a._none, b._all, b._none)
					then
						bad[key] = bad[key] or {}
						local a_old = honor_pre and is_pre_existing(key, a)
						local b_old = honor_pre and is_pre_existing(key, b)
						if a_old and not b_old then
							bad[key][b] = true
						elseif b_old and not a_old then
							bad[key][a] = true
						else
							bad[key][a] = true
							bad[key][b] = true
						end
						local o1, o2 = a.owner or "?", b.owner or "?"
						if o2 < o1 then
							o1, o2 = o2, o1
						end
						diags[#diags + 1] = ("ambiguous key binding for %s in %s (%s, priority %s): owners %s and %s conflict — neither takes effect"):format(
							key,
							a.context,
							layer_name(a.rank),
							tostring(a.priority or 0),
							o1,
							o2
						)
					end
				end
			end
		end
	end
	return bad, diags
end

local function effective_entries_in_context(m, ctx)
	local info = km_host and km_host.context_info and km_host.context_info(ctx)
	local chain = info and info.chain or { ctx }
	local by_key = {}
	for _, key in ipairs(sorted_map_keys(m)) do
		local st = m[key] or {}
		local active = {}
		local blocked = 0
		for _, ctx_i in ipairs(chain) do
			if blocked == 0xFFFF then
				break
			end
			for idx = #st, 1, -1 do
				local e = st[idx]
				if e.context == ctx_i then
					local sm = state_mask(e._all or 0, e._none or 0)
					if e.action == false then
						blocked = blocked | sm
					elseif (sm & (~blocked & 0xFFFF)) ~= 0 then
						active[#active + 1] = e
						local declines = fns[e.action] ~= nil or actions[e.action] ~= nil
						if not declines then
							blocked = blocked | sm
						end
					end
				end
			end
		end
		if #active > 0 then
			by_key[key] = active
		end
	end
	return by_key
end

local function effective_own_entries(m, ctx)
	local by_key = {}
	for _, key in ipairs(sorted_map_keys(m)) do
		local st = m[key] or {}
		local active = {}
		local blocked = 0
		for idx = #st, 1, -1 do
			local e = st[idx]
			if e.context == ctx then
				local sm = state_mask(e._all or 0, e._none or 0)
				if e.action == false then
					blocked = blocked | sm
				elseif (sm & (~blocked & 0xFFFF)) ~= 0 then
					active[#active + 1] = e
					local declines = fns[e.action] ~= nil or actions[e.action] ~= nil
					if not declines then
						blocked = blocked | sm
					end
				end
			end
		end
		if #active > 0 then
			by_key[key] = active
		end
	end
	return by_key
end

local function find_sequence_conflicts(m, honor_pre)
	local bad, diags, seen = {}, {}, {}
	local function scan(ctx, by_key)
		for _, seq_key in ipairs(sorted_map_keys(by_key)) do
			if string.find(seq_key, " ", 1, true) then
				local tokens = {}
				for tok in string.gmatch(seq_key, "%S+") do
					tokens[#tokens + 1] = tok
				end
				local seq_entries = by_key[seq_key]
				for plen = 1, #tokens - 1 do
					local prefix = table.concat(tokens, " ", 1, plen)
					local pref_entries = by_key[prefix]
					if pref_entries then
						for _, se in ipairs(seq_entries) do
							for _, pe in ipairs(pref_entries) do
								if when_overlaps(se._all, se._none, pe._all, pe._none) then
									local se_old = honor_pre and is_pre_existing(seq_key, se)
									local pe_old = honor_pre and is_pre_existing(prefix, pe)
									if se_old and not pe_old then
										bad[prefix] = bad[prefix] or {}
										bad[prefix][pe] = true
									elseif pe_old and not se_old then
										bad[seq_key] = bad[seq_key] or {}
										bad[seq_key][se] = true
									else
										bad[seq_key] = bad[seq_key] or {}
										bad[seq_key][se] = true
										bad[prefix] = bad[prefix] or {}
										bad[prefix][pe] = true
									end
									local msg = ("ambiguous key binding: `%s` (%s) is also the start of `%s` (%s) in %s — neither takes effect"):format(
										prefix,
										tostring(pe.action),
										seq_key,
										tostring(se.action),
										ctx
									)
									if not seen[msg] then
										seen[msg] = true
										diags[#diags + 1] = msg
									end
								end
							end
						end
					end
				end
			end
		end
	end
	for _, ctx in ipairs(all_contexts()) do
		local info = km_host and km_host.context_info and km_host.context_info(ctx)
		if info and info.active then
			scan(ctx, effective_entries_in_context(m, ctx))
		else
			scan(ctx, effective_own_entries(m, ctx))
		end
	end
	return bad, diags
end

local function remove_bad_entries(bad)
	local dirty = {}
	for key, set_entries in pairs(bad) do
		local st = maps[key]
		if st then
			for i = #st, 1, -1 do
				if set_entries[st[i]] then
					local e = table.remove(st, i)
					release(e.action)
					dirty[key] = true
				end
			end
		end
	end
	for key in pairs(dirty) do
		rebind(key)
	end
end

local function handle_entries(h)
	if type(h) ~= "table" then
		return {}
	end
	if type(h.records) == "table" then
		local out = {}
		for i, rec in ipairs(h.records) do
			out[i] = rec.entry
		end
		return out
	end
	return h.entries or (h.entry and { h.entry } or {})
end

local function clone_maps_with_stage(stage, previous_keymaps)
	local prev_set = {}
	for _, h in ipairs(previous_keymaps or {}) do
		for _, entry in ipairs(handle_entries(h)) do
			prev_set[entry] = true
		end
	end
	local proj = {}
	for key, st in pairs(maps) do
		local copy = {}
		for _, e in ipairs(st) do
			if not prev_set[e] then
				copy[#copy + 1] = e
			end
		end
		if #copy > 0 then
			proj[key] = copy
		end
	end
	local fake_seq = bind_seq
	for _, stage_entry in ipairs(stage or {}) do
		if stage_entry._keymap_stage and not (stage_entry.handle and stage_entry.handle._cancelled) then
			for _, item in ipairs(stage_entry.items or {}) do
				local st = proj[item.key] or {}
				proj[item.key] = st
				local c = item.context
				local all, none = item._all, item._none
				for i = #st, 1, -1 do
					local e = st[i]
					if
						e.context == c
						and e.rank == item.rank
						and e.owner == item.owner
						and when_overlaps(e._all, e._none, all, none)
					then
						table.remove(st, i)
					end
				end
				fake_seq = fake_seq + 1
				insert_entry(st, {
					context = c,
					action = item.action_id or item.action,
					desc = item.desc,
					when = item.when,
					rank = item.rank,
					priority = item.priority or 0,
					owner = item.owner,
					seq = fake_seq,
					_all = all,
					_none = none,
				})
			end
		end
	end
	return proj
end

local function make_handle(first_key, pending)
	local handle = {
		key = first_key,
		entry = nil,
		entries = {},
		records = {},
		_pending = pending == true,
		_cancelled = false,
		_detach = nil,
	}
	function handle.remove(self)
		mare.keymap.remove(self or handle)
	end
	return handle
end

-- Commit already-normalized and validated `expanded` records into `maps`, populate the unified
-- `handle`, and flush all touched `(context, key)` stacks in one batch.
local function commit_prepared(expanded, rank, owner, layer, handle)
	local bound_records, bound_entries = {}, {}
	local dirty_order, dirty_seen = {}, {}
	local fn_ids = {}
	for _, item in ipairs(expanded) do
		local id = item.action
		if type(id) == "function" then
			local fn_id = fn_ids[id]
			if not fn_id then
				fn_seq = fn_seq + 1
				fn_id = "fn:" .. fn_seq
				fns[fn_id] = id
				fn_ids[id] = fn_id
			end
			id = fn_id
		end
		-- Retain the incoming callback before releasing replaced same-owner entries so a
		-- duplicate/normalized key binding the same function in one batch cannot drop its
		-- refcount to zero and clear `fns[id]`.
		retain(id)
		local st = maps[item.key] or {}
		maps[item.key] = st
		-- Same-owner rebinding replaces its own entry instead of stacking when `when` overlaps.
		for idx = #st, 1, -1 do
			local prev = st[idx]
			if
				prev.context == item.context
				and prev.rank == rank
				and prev.owner == owner
				and when_overlaps(prev._all, prev._none, item._all, item._none)
			then
				table.remove(st, idx)
				release(prev.action)
			end
		end
		bind_seq = bind_seq + 1
		local entry = {
			context = item.context,
			action = id,
			desc = item.desc,
			when = item.when,
			rank = rank,
			priority = item.priority,
			owner = owner,
			layer = layer,
			seq = bind_seq,
			_all = item._all,
			_none = item._none,
		}
		insert_entry(st, entry)
		bound_records[#bound_records + 1] = { key = item.key, entry = entry }
		bound_entries[#bound_entries + 1] = entry
		local dkey = item.context .. "\0" .. item.key
		if not dirty_seen[dkey] then
			dirty_seen[dkey] = true
			dirty_order[#dirty_order + 1] = { item.context, item.key }
		end
	end
	handle.records = bound_records
	handle.entries = bound_entries
	handle.entry = bound_entries[1]
	handle.key = bound_records[1] and bound_records[1].key or handle.key
	handle._pending = false
	local updates = {}
	for i, pair in ipairs(dirty_order) do
		updates[i] = { pair[1], pair[2], build_binding_list(maps[pair[2]], pair[1]) }
	end
	flush_updates(updates)
end

-- Unified binding registration core shared by `mare.keymap.set` (single-spec batch) and
-- `mare.keymap.set_many` (multi-spec batch). Validates atomically, handles both config-evaluation
-- staging and immediate runtime commits, and always returns the same handle shape.
local function set_batch(fn_name, expanded, rank, owner, layer, explicit_source, level)
	if #expanded == 0 then
		return make_handle(nil, false)
	end

	local raise = err_level(level)

	-- Normalize and validate every `(context, key, when, action, printable)` in one Rust lock.
	local prep_req = {}
	for i, item in ipairs(expanded) do
		prep_req[i] = {
			context = item.context,
			key = item.raw_key,
			when = item.when,
			action = type(item.action) == "string" and item.action or nil,
			check_printable = rank == 1 and item.action ~= false,
		}
	end
	local prepared, prep_err = km_host.prepare_batch(prep_req)
	if not prepared then
		error(("mare.keymap.%s: %s"):format(fn_name, tostring(prep_err)), raise)
	end

	for i, item in ipairs(expanded) do
		check_action_context(fn_name, item.action, item.context, raise)
		item.key = prepared[i].key
		item._all = prepared[i].all
		item._none = prepared[i].none
	end

	local stage = in_config() and config_stage()
	if stage then
		local handle = make_handle(expanded[1].key, true)
		local stage_items = {}
		for i, item in ipairs(expanded) do
			stage_items[i] = {
				key = item.key,
				context = item.context,
				_all = item._all,
				_none = item._none,
				action = item.action,
				action_id = type(item.action) == "function" and "fn:pending" or item.action,
				desc = item.desc,
				when = item.when,
				unique = item.unique,
				rank = rank,
				owner = owner,
				layer = layer,
				priority = item.priority,
			}
		end
		local stage_entry = {
			_keymap_stage = true,
			items = stage_items,
			handle = handle,
		}
		local function validate_staged()
			if handle._cancelled then
				return
			end
			local previous = {}
			local st = config_state()
			for _, h in ipairs((st and st.config_previous_keymaps) or {}) do
				for _, entry in ipairs(handle_entries(h)) do
					previous[entry] = true
				end
			end
			local seen_in_batch = {}
			for _, item in ipairs(stage_items) do
				if item.action ~= false and item.unique then
					for _, entry in ipairs(maps[item.key] or {}) do
						if not previous[entry] and entry.context == item.context then
							error(
								("mare.keymap.%s: %s is already bound in context %s (unique = true)"):format(
									fn_name,
									item.key,
									item.context
								),
								raise
							)
						end
					end
					for _, other in ipairs(stage) do
						if other == stage_entry then
							break
						end
						if other._keymap_stage and not (other.handle and other.handle._cancelled) then
							for _, oitem in ipairs(other.items or {}) do
								if oitem.key == item.key and oitem.context == item.context then
									error(
										("mare.keymap.%s: %s is already bound in context %s (unique = true)"):format(
											fn_name,
											item.key,
											item.context
										),
										raise
									)
								end
							end
						end
					end
					if seen_in_batch[item.context .. "\0" .. item.key] then
						error(
							("mare.keymap.%s: %s is already bound in context %s (unique = true)"):format(
								fn_name,
								item.key,
								item.context
							),
							raise
						)
					end
				end
				seen_in_batch[item.context .. "\0" .. item.key] = true
			end
		end
		stage_entry.validate = validate_staged
		stage_entry.commit = function()
			if handle._cancelled then
				return
			end
			commit_prepared(expanded, rank, owner, layer, handle)
		end
		validate_staged()
		table.insert(stage, stage_entry)
		local st = config_state()
		if st and st.config_keymaps then
			table.insert(st.config_keymaps, handle)
		end
		return handle
	end

	-- Check `unique = true` against existing stacks and earlier items in this batch.
	local seen_batch = {}
	for _, item in ipairs(expanded) do
		if item.action ~= false and item.unique then
			for _, e in ipairs(maps[item.key] or {}) do
				if e.context == item.context then
					error(
						("mare.keymap.%s: %s is already bound in context %s (unique = true)"):format(
							fn_name,
							item.key,
							item.context
						),
						raise
					)
				end
			end
			if seen_batch[item.context .. "\0" .. item.key] then
				error(
					("mare.keymap.%s: %s is already bound in context %s (unique = true)"):format(
						fn_name,
						item.key,
						item.context
					),
					raise
				)
			end
		end
		seen_batch[item.context .. "\0" .. item.key] = true
	end

	-- When settled outside config, validate ambiguity and sequence conflicts atomically before mutating `maps`.
	if settled and not in_config() then
		local fake_items = {}
		for _, item in ipairs(expanded) do
			if item.action ~= false then
				for _, e in ipairs(maps[item.key] or {}) do
					if
						e.action ~= false
						and e.context == item.context
						and e.rank == rank
						and (e.priority or 0) == item.priority
						and e.owner ~= owner
						and when_overlaps(e._all, e._none, item._all, item._none)
					then
						local o1, o2 = owner, e.owner or "?"
						if o2 < o1 then
							o1, o2 = o2, o1
						end
						error(
							("mare.keymap.%s: ambiguous key binding for %s in %s (%s, priority %s): owners %s and %s conflict — neither takes effect"):format(
								fn_name,
								item.key,
								item.context,
								layer,
								tostring(item.priority),
								o1,
								o2
							),
							raise
						)
					end
				end
			end
			fake_items[#fake_items + 1] = {
				key = item.key,
				context = item.context,
				_all = item._all,
				_none = item._none,
				action = type(item.action) == "function" and "fn:pending" or item.action,
				desc = item.desc,
				when = item.when,
				rank = rank,
				owner = owner,
				layer = layer,
				priority = item.priority,
			}
		end
		local proj = clone_maps_with_stage({ { _keymap_stage = true, items = fake_items } }, nil)
		local _, diags_seq = find_sequence_conflicts(proj)
		if #diags_seq > 0 then
			error(("mare.keymap.%s: %s"):format(fn_name, diags_seq[1]), raise)
		end
	end

	local handle = make_handle(expanded[1].key, false)
	commit_prepared(expanded, rank, owner, layer, handle)

	-- Bindings from init.lua are tracked so /reload config can drop them before re-running it;
	-- direct plugin bindings are attached to the active plugin owner so unload/disable drops them.
	local cfg_st = in_config() and config_state() or nil
	if cfg_st and cfg_st.config_keymaps then
		table.insert(cfg_st.config_keymaps, handle)
	elseif not explicit_source and km_host and km_host.attach_disposer then
		handle._detach = km_host.attach_disposer(function()
			handle._detach = nil
			pcall(mare.keymap.remove, handle)
		end)
	end
	return handle
end

--- opts (Neovim `vim.keymap.set`): `desc`; `unique = true` refuses a key already bound in one
--- of the contexts (an error, like Neovim); `priority` breaks ties within the same layer;
--- `when` = a predicate or a list of them, all of which must hold for the binding to take the key
--- ("editor_empty", "completion_open", "paste_burst", "working"; "!x" negates). `context` may be
--- a list of contexts. The returned handle undoes the binding: `handle:remove()` or
--- `mare.keymap.remove(handle)`.
local function set(context, key, action, opts, src)
	opts = opts or {}
	local level = src and 0 or 3
	local rank, owner, layer, explicit_source
	if src then
		rank, owner, layer, explicit_source = src.rank, src.owner, src.layer, true
	else
		rank, owner, layer, explicit_source = resolve_source("set", opts, 3)
	end
	local priority = parse_priority(opts, "set", level)
	if type(action) == "function" and (type(opts.desc) ~= "string" or not opts.desc:match("%S")) then
		error("mare.keymap.set: anonymous function binding requires a non-empty `desc`", level)
	end
	if type(key) ~= "string" then
		error("mare.keymap.set: key must be a string", level)
	end
	local contexts = type(context) == "table" and context or { context or "editor" }
	if #contexts == 0 then
		contexts = { "editor" }
	end
	local expanded = {}
	for i, c in ipairs(contexts) do
		expanded[i] = {
			context = tostring(c),
			raw_key = key,
			action = action,
			when = opts.when,
			desc = opts.desc,
			unique = opts.unique == true,
			priority = priority,
		}
	end
	return set_batch("set", expanded, rank, owner, layer, explicit_source, level)
end

function mare.keymap.set(context, key, action, opts)
	if action == false and opts and opts.when ~= nil then
		error("mare.keymap.set: an unbinding (false) takes no `when`", 2)
	end
	return set(context, key, action, opts)
end

--- Register multiple bindings in one batch. Each item in `specs` is either:
---   `{ context, key_or_keys, action, opts? }` or `{ context = ..., key = ... (or keys = ...), action = ..., when?, desc?, priority?, unique? }`.
--- Validates atomically (no partial bindings on error), flushes all updated `(context, key)` stacks
--- in a single host batch call, and returns one handle whose `:remove()` (or `mare.keymap.remove(handle)`)
--- removes every binding in the batch.
function mare.keymap.set_many(specs, batch_opts)
	if type(specs) ~= "table" then
		error("mare.keymap.set_many: specs must be a list of binding records", 2)
	end
	if batch_opts ~= nil and type(batch_opts) ~= "table" then
		error("mare.keymap.set_many: opts must be a table", 2)
	end
	local rank, owner, layer, explicit_source = resolve_source("set_many", batch_opts, 2)
	local default_priority = parse_priority(batch_opts, "set_many", 2)
	local default_when = batch_opts and batch_opts.when or nil
	local default_desc = batch_opts and batch_opts.desc or nil
	local default_unique = batch_opts and batch_opts.unique or false

	-- Expand all specs into flat `(context, raw_key, action, when, desc, unique, priority)` records.
	local expanded = {}
	for idx, spec in ipairs(specs) do
		if type(spec) ~= "table" then
			error(("mare.keymap.set_many: item #%d must be a table"):format(idx), 2)
		end
		local ctx_spec = spec.context ~= nil and spec.context or spec[1]
		local key_spec = spec.keys ~= nil and spec.keys or (spec.key ~= nil and spec.key or spec[2])
		local action = spec.action ~= nil and spec.action or spec[3]
		local item_opts = spec[4]
		if item_opts ~= nil and type(item_opts) ~= "table" then
			error(("mare.keymap.set_many: item #%d opts must be a table"):format(idx), 2)
		end
		local when = spec.when
		if when == nil and item_opts then
			when = item_opts.when
		end
		if when == nil then
			when = default_when
		end
		local desc = spec.desc
		if desc == nil and item_opts then
			desc = item_opts.desc
		end
		if desc == nil then
			desc = default_desc
		end
		local unique = spec.unique
		if unique == nil and item_opts then
			unique = item_opts.unique
		end
		if unique == nil then
			unique = default_unique
		end
		local prio_raw = spec.priority
		if prio_raw == nil and item_opts then
			prio_raw = item_opts.priority
		end
		local priority = prio_raw ~= nil and parse_priority({ priority = prio_raw }, "set_many", 2) or default_priority
		if action == false and when ~= nil then
			error("mare.keymap.set_many: an unbinding (false) takes no `when`", 2)
		end
		if type(action) == "function" and (type(desc) ~= "string" or not desc:match("%S")) then
			error("mare.keymap.set_many: anonymous function binding requires a non-empty `desc`", 2)
		end
		local contexts = type(ctx_spec) == "table" and ctx_spec or { ctx_spec or "editor" }
		if #contexts == 0 then
			contexts = { "editor" }
		end
		local keys = type(key_spec) == "table" and key_spec or { key_spec }
		if #keys == 0 then
			error(("mare.keymap.set_many: item #%d requires at least one key"):format(idx), 2)
		end
		for _, raw_key in ipairs(keys) do
			if type(raw_key) ~= "string" then
				error(("mare.keymap.set_many: item #%d key must be a string"):format(idx), 2)
			end
			for _, c in ipairs(contexts) do
				expanded[#expanded + 1] = {
					context = tostring(c),
					raw_key = raw_key,
					action = action,
					when = when,
					desc = desc,
					unique = unique == true,
					priority = priority,
				}
			end
		end
	end

	return set_batch("set_many", expanded, rank, owner, layer, explicit_source, 2)
end

local function config_validate_stage(stage, previous_keymaps)
	local proj = clone_maps_with_stage(stage, previous_keymaps)
	local _, ambig_diags = find_ambiguity_conflicts(proj)
	if #ambig_diags > 0 then
		error("mare.keymap: " .. ambig_diags[1], 0)
	end
	local _, seq_diags = find_sequence_conflicts(proj)
	if #seq_diags > 0 then
		error("mare.keymap: " .. seq_diags[1], 0)
	end
end

local function snapshot_state()
	local m_copy, f_copy, r_copy, a_copy, entries, sigs = {}, {}, {}, {}, {}, {}
	for key, st in pairs(maps) do
		local copy = {}
		for i, e in ipairs(st) do
			copy[i] = e
			if e.action ~= false then
				entries[e] = true
				local sig = ("%s\0%s\0%s\0%d\0%s\0%d\0%d"):format(
					key,
					e.context,
					e.owner or "",
					e.rank or 0,
					tostring(e.priority or 0),
					e._all or 0,
					e._none or 0
				)
				sigs[sig] = true
			end
		end
		m_copy[key] = copy
	end
	for k, v in pairs(fns) do
		f_copy[k] = v
	end
	for k, v in pairs(refs) do
		r_copy[k] = v
	end
	for name, st in pairs(actions) do
		local copy = {}
		for i, a in ipairs(st) do
			copy[i] = a
		end
		a_copy[name] = copy
	end
	return {
		maps = m_copy,
		fns = f_copy,
		refs = r_copy,
		actions = a_copy,
		n = fn_seq,
		seq = bind_seq,
		pre_entries = entries,
		pre_sigs = sigs,
	}
end

local function restore_state(snap)
	if not snap then
		return
	end
	local all_keys = {}
	for k in pairs(maps) do
		all_keys[k] = true
	end
	for k in pairs(snap.maps) do
		all_keys[k] = true
	end
	maps = {}
	for k, st in pairs(snap.maps) do
		local copy = {}
		for i, e in ipairs(st) do
			copy[i] = e
		end
		maps[k] = copy
	end
	fns = {}
	for k, v in pairs(snap.fns) do
		fns[k] = v
	end
	refs = {}
	for k, v in pairs(snap.refs) do
		refs[k] = v
	end
	actions = {}
	for name, st in pairs(snap.actions) do
		local copy = {}
		for i, a in ipairs(st) do
			copy[i] = a
		end
		actions[name] = copy
	end
	fn_seq = snap.n
	bind_seq = snap.seq
	for k in pairs(all_keys) do
		rebind(k)
	end
end

local function begin_reload()
	reload_depth = reload_depth + 1
	if reload_depth == 1 then
		local snap = snapshot_state()
		reload_snap = snap
		pre_entries = snap.pre_entries
		pre_sigs = snap.pre_sigs
		settled = false
	end
end

local function rollback_reload()
	reload_depth = math.max(0, reload_depth - 1)
	if reload_depth == 0 then
		local snap = reload_snap
		reload_snap = nil
		pre_entries = nil
		pre_sigs = nil
		if snap then
			restore_state(snap)
		end
		settled = true
	end
end

local function rollback_owners_from_snap(snap, bad_owners)
	if not snap or not next(bad_owners) then
		return
	end
	local pre = snap.pre_entries or {}
	local dirty = {}
	for key, st in pairs(maps) do
		for i = #st, 1, -1 do
			local e = st[i]
			if e.owner and bad_owners[e.owner] and not pre[e] then
				table.remove(st, i)
				release(e.action)
				dirty[key] = true
			end
		end
	end
	for key, st in pairs(snap.maps) do
		for _, e in ipairs(st) do
			if e.owner and bad_owners[e.owner] then
				local cur = maps[key] or {}
				maps[key] = cur
				if not table.find(cur, e) then
					if snap.fns[e.action] then
						fns[e.action] = snap.fns[e.action]
					end
					insert_entry(cur, e)
					retain(e.action)
					dirty[key] = true
				end
			end
		end
	end
	for name, st in pairs(actions) do
		for i = #st, 1, -1 do
			if st[i].owner and bad_owners[st[i].owner] then
				table.remove(st, i)
			end
		end
		if #st == 0 then
			actions[name] = nil
		end
	end
	for name, st in pairs(snap.actions) do
		for _, a in ipairs(st) do
			if a.owner and bad_owners[a.owner] then
				local cur = actions[name] or {}
				actions[name] = cur
				if not table.find(cur, a) then
					cur[#cur + 1] = a
				end
			end
		end
	end
	for key in pairs(dirty) do
		rebind(key)
	end
end

local function settle(strict)
	local diagnostics = {}
	local snap = reload_snap
	local honor_pre = pre_sigs ~= nil
	local bad_ambig, ambig_diags = find_ambiguity_conflicts(maps, honor_pre)
	for _, msg in ipairs(ambig_diags) do
		diagnostics[#diagnostics + 1] = msg
	end

	local bad_invalid = {}
	for _, key in ipairs(sorted_map_keys(maps)) do
		for _, e in ipairs(maps[key] or {}) do
			if type(e.action) == "string" and not fns[e.action] then
				local ok, err = pcall(function()
					if km_host and km_host.is_core_action and km_host.is_core_action(e.action) then
						return
					end
					local a = named_action(e.action)
					if not a then
						error(("unknown action %q bound to %s in %s"):format(e.action, key, e.context), 0)
					end
					if type(a.contexts) == "table" and #a.contexts > 0 and km_host and km_host.context_info then
						local info = km_host.context_info(e.context)
						local chain = info and info.chain or {}
						local matches = false
						for _, allowed in ipairs(a.contexts) do
							if table.find(chain, allowed) then
								matches = true
								break
							end
						end
						if not matches then
							error(
								("action %q is not available in context %s (bound to %s)"):format(
									e.action,
									e.context,
									key
								),
								0
							)
						end
					end
				end)
				if not ok then
					bad_invalid[key] = bad_invalid[key] or {}
					bad_invalid[key][e] = true
					diagnostics[#diagnostics + 1] = "mare.keymap: " .. tostring(err)
				end
			end
		end
	end

	local bad_seq, seq_diags = find_sequence_conflicts(maps, honor_pre)
	for _, msg in ipairs(seq_diags) do
		diagnostics[#diagnostics + 1] = msg
	end

	if #diagnostics > 0 then
		if strict and snap then
			restore_state(snap)
		elseif honor_pre and snap then
			local bad_owners = {}
			for _, tbl in ipairs({ bad_ambig, bad_invalid, bad_seq }) do
				for _, eset in pairs(tbl) do
					for e in pairs(eset) do
						if e.owner then
							bad_owners[e.owner] = true
						end
					end
				end
			end
			rollback_owners_from_snap(snap, bad_owners)
		else
			remove_bad_entries(bad_ambig)
			remove_bad_entries(bad_invalid)
			remove_bad_entries(bad_seq)
		end
	end

	reload_depth = 0
	reload_snap = nil
	pre_entries = nil
	pre_sigs = nil
	settled = true
	return diagnostics
end

local function finish_reload(strict)
	reload_depth = math.max(0, reload_depth - 1)
	if reload_depth > 0 then
		return {}
	end
	return settle(strict)
end

---------------------------------------------------------------------------
-- `mare.setup { keys = ... }` shorthand (config eval + runtime option/changed("keys"))
---------------------------------------------------------------------------
local setup_key_entries = {}
local last_staged_failed_keys = nil

local function report_setup_keys_error(msg)
	if mare.log then
		pcall(mare.log, "error", msg)
	end
	if ui and ui.notify then
		pcall(ui.notify, msg, "error")
	end
end

local function resolve_setup_key_source(raw_key, sources)
	local src_layer
	if type(sources) == "table" then
		local specific = sources["keys." .. raw_key]
		if type(specific) == "table" and type(specific.layer) == "string" then
			src_layer = specific.layer
		else
			local general = sources["keys"]
			if type(general) == "table" and type(general.layer) == "string" then
				src_layer = general.layer
			end
		end
	end
	if src_layer == "project" then
		return 4, "project", "project"
	elseif src_layer == "default" then
		return 1, "default", "default"
	end
	return 3, "user", "user"
end

local function apply_setup_keys_now(keys_tbl, sources)
	-- Two spellings of one key with different actions ("<C-o>" and "ctrl+o") are ambiguous:
	-- report an error and bind neither.
	local by_key, names = {}, {}
	if type(keys_tbl) == "table" then
		for key, action in pairs(keys_tbl) do
			if type(key) == "string" then
				local k = normalize_seq(key)
				by_key[k] = by_key[k] or {}
				table.insert(by_key[k], { key = key, action = action })
			end
		end
	end
	for k in pairs(by_key) do
		names[#names + 1] = k
	end
	table.sort(names)

	local desired = {}
	for _, k in ipairs(names) do
		local list = by_key[k]
		local seen_actions = {}
		for _, e in ipairs(list) do
			seen_actions[tostring(e.action)] = true
		end
		if next(seen_actions, next(seen_actions)) ~= nil then
			local spell = {}
			for _, e in ipairs(list) do
				spell[#spell + 1] = ("%s = %s"):format(e.key, tostring(e.action))
			end
			table.sort(spell)
			report_setup_keys_error(
				("ambiguous key binding in `keys`: %s are the same key — neither is bound"):format(
					table.concat(spell, ", ")
				)
			)
			return false
		end
		local chosen = list[1]
		local rank, owner, layer = resolve_setup_key_source(chosen.key, sources)
		local act = (chosen.action == false or chosen.action == "none") and false or chosen.action
		desired[k] = {
			raw_key = chosen.key,
			action = act,
			rank = rank,
			owner = owner,
			layer = layer,
		}
	end

	-- Fast path: skip if desired set matches currently applied handles exactly.
	local same = true
	for k, prev in pairs(setup_key_entries) do
		local d = desired[k]
		if
			not d
			or d.action ~= prev.action
			or d.rank ~= prev.rank
			or d.owner ~= prev.owner
			or d.layer ~= prev.layer
		then
			same = false
			break
		end
	end
	if same then
		for k in pairs(desired) do
			if not setup_key_entries[k] then
				same = false
				break
			end
		end
	end
	if same then
		return true
	end

	local snap = snapshot_state()
	local next_entries = {}
	for k, prev in pairs(setup_key_entries) do
		local d = desired[k]
		if
			d
			and d.action == prev.action
			and d.rank == prev.rank
			and d.owner == prev.owner
			and d.layer == prev.layer
		then
			next_entries[k] = prev
		else
			mare.keymap.remove(prev.handle)
		end
	end

	local first_err
	for _, k in ipairs(names) do
		if not next_entries[k] then
			local d = desired[k]
			local ok, h_or_err = pcall(set, "editor", d.raw_key, d.action, nil, {
				rank = d.rank,
				owner = d.owner,
				layer = d.layer,
			})
			if not ok then
				first_err = tostring(h_or_err)
				break
			end
			next_entries[k] = {
				raw_key = d.raw_key,
				action = d.action,
				rank = d.rank,
				owner = d.owner,
				layer = d.layer,
				handle = h_or_err,
			}
		end
	end

	if first_err then
		restore_state(snap)
		report_setup_keys_error(first_err)
		return false
	end

	setup_key_entries = next_entries
	return true
end

local function apply_config_keys(keys_tbl, sources, staged)
	if staged then
		local stage = config_stage()
		if type(stage) == "table" then
			table.insert(stage, {
				commit = function()
					local ok = apply_setup_keys_now(keys_tbl, sources)
					if ok then
						last_staged_failed_keys = nil
					else
						last_staged_failed_keys = keys_tbl
					end
				end,
			})
			return
		end
	end
	if last_staged_failed_keys ~= nil and rawequal(keys_tbl, last_staged_failed_keys) then
		last_staged_failed_keys = nil
		return
	end
	last_staged_failed_keys = nil
	apply_setup_keys_now(keys_tbl, sources)
end

--- del(context, key, opts?) -> boolean, string: remove the topmost binding of key in context
--- owned by the caller (or `opts.owner` / `opts.layer`), returning whether a binding was removed
--- and the normalized key sequence.
function mare.keymap.del(context, key, opts)
	context = context or "editor"
	key = normalize_seq(key)
	check("del", context, key, nil, nil, 2)
	local rank, owner = resolve_source("del", opts, 2)
	local st = maps[key] or {}
	local removed = false
	for i = #st, 1, -1 do
		if st[i].context == context and st[i].rank == rank and st[i].owner == owner then
			local e = table.remove(st, i)
			release(e.action)
			removed = true
			break
		end
	end
	if removed then
		rebind(key, { context })
	end
	return removed, key
end

--- remove(handle): remove every binding registered by a handle from `set` or `set_many`
function mare.keymap.remove(handle)
	if type(handle) ~= "table" then
		return
	end
	if handle._detach then
		local detach = handle._detach
		handle._detach = nil
		pcall(detach)
	end
	if handle._pending then
		handle._cancelled = true
	end
	local records = handle.records
	if not records or #records == 0 then
		return
	end
	local dirty_order, dirty_seen = {}, {}
	for _, rec in ipairs(records) do
		local st = maps[rec.key]
		if st then
			local i = table.find(st, rec.entry)
			if i then
				table.remove(st, i)
				release(rec.entry.action)
				if #st == 0 then
					maps[rec.key] = nil
				end
				local dkey = rec.entry.context .. "\0" .. rec.key
				if not dirty_seen[dkey] then
					dirty_seen[dkey] = true
					dirty_order[#dirty_order + 1] = { rec.entry.context, rec.key }
				end
			end
		end
	end
	local updates = {}
	for i, pair in ipairs(dirty_order) do
		updates[i] = { pair[1], pair[2], build_binding_list(maps[pair[2]], pair[1]) }
	end
	flush_updates(updates)
end

--- context(name, { parent }) -> handle: declare a plugin subcontext (`name` = `parent .. "." .. seg`).
--- Removing the handle deletes the context, its descendants, and all bindings in them.
function mare.keymap.context(name, opts)
	if type(name) ~= "string" or type(opts) ~= "table" or type(opts.parent) ~= "string" then
		error("mare.keymap.context: expected (name, { parent = string })", 2)
	end
	local ctx_opts = {
		swallow_printable = opts.swallow_printable == true,
		active = opts.active == true,
	}
	local st = in_config() and config_state() or nil
	local stage = st and st.stage or nil
	local prev_list = stage and st.config_previous_contexts or nil
	local reused = false
	if stage then
		for _, c in ipairs(st.config_contexts or {}) do
			if c.name == name then
				error(('mare.keymap.context: context "%s" already exists'):format(name), 2)
			end
		end
		if prev_list then
			for _, c in ipairs(prev_list) do
				if c.name == name then
					reused = true
					break
				end
			end
		end
	end
	if reused then
		local ok, _, _, err = pcall(km_host.check, opts.parent, "<F2>", nil, nil)
		if not ok or err then
			error(("mare.keymap.context: %s"):format(tostring(err or "invalid parent")), 2)
		end
		local seg = name:sub(#opts.parent + 2)
		if name:sub(1, #opts.parent + 1) ~= opts.parent .. "." or not seg:match("^[a-z][a-z0-9_-]*$") then
			error(('mare.keymap.context: invalid context "%s" under "%s"'):format(name, opts.parent), 2)
		end
		km_host.context(name, { active = ctx_opts.active })
	else
		local ok, err = pcall(km_host.context, name, opts.parent, ctx_opts)
		if not ok then
			error(("mare.keymap.context: %s"):format(tostring(err)), 2)
		end
	end
	local detach
	local handle = { name = name, _removed = false }
	function handle.set_active(first, second)
		if handle._removed then
			return false
		end
		local on
		if first == handle then
			on = second
		else
			on = first
		end
		return km_host.context(name, { active = on == true })
	end
	function handle.is_active()
		if handle._removed then
			return false
		end
		return km_host.context(name, {})
	end
	function handle.remove()
		if handle._removed then
			return
		end
		handle._removed = true
		if detach then
			local fn = detach
			detach = nil
			pcall(fn)
		end
		local removed = km_host.context(name, nil)
		if not removed or #removed == 0 then
			return
		end
		local gone = {}
		for _, c in ipairs(removed) do
			gone[c] = true
		end
		local dirty = {}
		for key, st in pairs(maps) do
			for i = #st, 1, -1 do
				if gone[st[i].context] then
					local e = table.remove(st, i)
					release(e.action)
					dirty[key] = true
				end
			end
		end
		for key in pairs(dirty) do
			rebind(key)
		end
	end
	local cur_st = in_config() and config_state() or nil
	if cur_st and cur_st.config_contexts then
		table.insert(cur_st.config_contexts, {
			name = name,
			parent = opts.parent,
			swallow_printable = ctx_opts.swallow_printable,
			active = ctx_opts.active,
			_added = not reused,
			handle = handle,
		})
	elseif km_host and km_host.attach_disposer then
		detach = km_host.attach_disposer(function()
			detach = nil
			pcall(handle.remove)
		end)
	end
	return handle
end

local function config_rollback_contexts(current)
	for i = #(current or {}), 1, -1 do
		local c = current[i]
		if c._added and c.handle then
			c.handle.remove()
		end
	end
end

local function config_commit_contexts(previous, current)
	local cur_by_name = {}
	for _, c in ipairs(current or {}) do
		cur_by_name[c.name] = c
	end
	local deleted = {}
	for i = #(previous or {}), 1, -1 do
		local p = previous[i]
		local c = cur_by_name[p.name]
		if not c or c.parent ~= p.parent or c.swallow_printable ~= p.swallow_printable then
			if p.handle then
				p.handle.remove()
			end
			deleted[p.name] = true
		end
	end
	for _, c in ipairs(current or {}) do
		if deleted[c.name] then
			km_host.context(c.name, c.parent, { swallow_printable = c.swallow_printable, active = c.active })
		end
	end
end

-- Capture only entries owned by the previous config evaluation. They can be restored
-- at their original stack positions if evaluating the replacement config fails.
local function config_snapshot(handles)
	local owned, saved = {}, {}
	for _, handle in ipairs(handles or {}) do
		for _, entry in ipairs(handle_entries(handle)) do
			owned[entry] = true
		end
	end
	for key, stack in pairs(maps) do
		for _, entry in ipairs(stack) do
			if owned[entry] then
				saved[#saved + 1] = { key = key, entry = entry, callback = fns[entry.action] }
			end
		end
	end
	return saved
end

local function config_restore(saved, current)
	for _, handle in ipairs(current or {}) do
		mare.keymap.remove(handle)
	end
	local keys = {}
	for _, item in ipairs(saved or {}) do
		local stack = maps[item.key] or {}
		maps[item.key] = stack
		if not table.find(stack, item.entry) then
			if item.callback then
				fns[item.entry.action] = item.callback
			end
			insert_entry(stack, item.entry)
			retain(item.entry.action)
		end
		keys[item.key] = true
	end
	for key in pairs(keys) do
		rebind(key)
	end
end

local active_config_actions = {}

local function config_actions_snapshot()
	local a_copy, f_copy, r_copy = {}, {}, {}
	for name, st in pairs(actions) do
		local copy = {}
		for i, a in ipairs(st) do
			copy[i] = a
		end
		a_copy[name] = copy
	end
	for k, v in pairs(fns) do
		f_copy[k] = v
	end
	for k, v in pairs(refs) do
		r_copy[k] = v
	end
	local prev = active_config_actions
	active_config_actions = {}
	return { actions = a_copy, fns = f_copy, refs = r_copy, prev = prev }
end

local function config_actions_commit(snap)
	for _, h in ipairs(snap and snap.prev or {}) do
		if h and h.remove then
			pcall(h.remove)
		end
	end
end

local function config_actions_restore(snap)
	for _, h in ipairs(active_config_actions) do
		if h and h.remove then
			pcall(h.remove)
		end
	end
	active_config_actions = snap and snap.prev or {}
	if not snap then
		return
	end
	actions = {}
	for name, st in pairs(snap.actions or {}) do
		local copy = {}
		for i, a in ipairs(st) do
			copy[i] = a
		end
		actions[name] = copy
	end
	for k, v in pairs(snap.fns or {}) do
		fns[k] = v
	end
	for k, v in pairs(snap.refs or {}) do
		refs[k] = v
	end
	for key in pairs(maps) do
		rebind(key)
	end
end

--- callback(action_id) -> the Lua function bound under that action id (from set), or nil
function mare.keymap.callback(id)
	return fns[id]
end

-- the core's copy says whether a binding's action is a Lua one: send the stacks that use `name`
-- again when it is registered or removed
local function rebind_users(name, skip)
	for key, st in pairs(maps) do
		if not skip[key] then
			for _, e in ipairs(st) do
				if e.action == name then
					rebind(key)
					break
				end
			end
		end
	end
end

-- the newest registration of a named action
local function named(name)
	return named_action(name)
end

--- action(name, fn, opts) -> handle: a named Lua action (what a key does; which key is the
--- binding's business: `mare.keymap.set(context, key, name)`), so users rebind it by name,
--- `hint(name, context)` finds its keys and lists show `opts.desc`. `fn(ev)` gets `{key, context}`;
--- returning exactly `false` declines: the key goes on to the binding below it, then the editor.
--- It runs at once, up to its first yield: decide before waiting on anything (a dialog, a
--- sleep), as the rest runs on its own. An error swallows the key and shows a notice.
--- Registering a name again shadows the previous action until the handle removes it.
--- `opts.keys` maps contexts to a key string, a `{ key = ..., when = ..., priority = ... }` record,
--- or a list of keys/records; removing the handle removes those default bindings too (leaving user bindings untouched).
function mare.keymap.action(name, fn, opts)
	if type(name) ~= "string" or type(fn) ~= "function" then
		error("mare.keymap.action: expected (name, function, opts?)", 2)
	end
	if opts ~= nil and type(opts) ~= "table" then
		error("mare.keymap.action: opts must be a table", 2)
	end
	local default_priority = parse_priority(opts, "action", 2)
	local function resolve_priority(p, fallback)
		if p == nil then
			return fallback
		end
		return parse_priority({ priority = p }, "action", 3)
	end
	local keys_opt = opts and opts.keys or nil
	if keys_opt ~= nil and type(keys_opt) ~= "table" then
		error("mare.keymap.action: opts.keys must be a table", 2)
	end
	local bind_specs = {}
	if keys_opt then
		local ctx_names = {}
		for ctx in pairs(keys_opt) do
			if type(ctx) ~= "string" then
				error("mare.keymap.action: opts.keys context must be a string", 2)
			end
			ctx_names[#ctx_names + 1] = ctx
		end
		table.sort(ctx_names)
		for _, ctx in ipairs(ctx_names) do
			local spec = keys_opt[ctx]
			if type(spec) == "string" then
				bind_specs[#bind_specs + 1] = { context = ctx, key = spec, priority = default_priority }
			elseif type(spec) == "table" then
				if
					spec.key ~= nil
					or spec.when ~= nil
					or spec.priority ~= nil
					or (next(spec) ~= nil and spec[1] == nil)
				then
					if type(spec.key) ~= "string" then
						error("mare.keymap.action: opts.keys entry requires a key string", 2)
					end
					bind_specs[#bind_specs + 1] = {
						context = ctx,
						key = spec.key,
						when = spec.when,
						priority = resolve_priority(spec.priority, default_priority),
					}
				else
					for _, entry in ipairs(spec) do
						if type(entry) == "string" then
							bind_specs[#bind_specs + 1] = { context = ctx, key = entry, priority = default_priority }
						elseif type(entry) == "table" then
							local k = entry.key or entry[1]
							if type(k) ~= "string" then
								error("mare.keymap.action: opts.keys entry requires a key string", 2)
							end
							local w = entry.when
							if w == nil then
								w = entry[2]
							end
							bind_specs[#bind_specs + 1] = {
								context = ctx,
								key = k,
								when = w,
								priority = resolve_priority(entry.priority, default_priority),
							}
						else
							error("mare.keymap.action: opts.keys entries must be strings or tables", 2)
						end
					end
				end
			else
				error("mare.keymap.action: opts.keys values must be key strings or lists", 2)
			end
		end
	end
	local rank, owner, layer, explicit_source = resolve_source("action", opts, 2)
	local src = { rank = rank, owner = owner, layer = layer }
	local st = actions[name] or {}
	actions[name] = st
	local a = { fn = fn, desc = opts and opts.desc, contexts = opts and opts.contexts, owner = owner }
	st[#st + 1] = a
	local key_handles = {}
	local function rollback_action()
		local i = table.find(st, a)
		if i then
			table.remove(st, i)
		end
		if #st == 0 and actions[name] == st then
			actions[name] = nil
		end
		local updated = {}
		for j = #key_handles, 1, -1 do
			local h = key_handles[j]
			for _, rec in ipairs(h.records or {}) do
				updated[rec.key] = true
			end
			mare.keymap.remove(h)
		end
		rebind_users(name, updated)
	end
	local updated = {}
	if #bind_specs > 0 then
		for _, b in ipairs(bind_specs) do
			local b_opts = {}
			if b.when ~= nil then
				b_opts.when = b.when
			end
			if b.priority ~= nil and b.priority ~= 0 then
				b_opts.priority = b.priority
			end
			local ok, h_or_err = pcall(set, b.context, b.key, name, next(b_opts) and b_opts or nil, src)
			if not ok then
				rollback_action()
				error(h_or_err, 2)
			end
			key_handles[#key_handles + 1] = h_or_err
			if not h_or_err._pending then
				updated[h_or_err.key] = true
			end
		end
	end
	rebind_users(name, updated)
	local detach
	local handle = { name = name, _removed = false }
	function handle.remove()
		if handle._removed then
			return
		end
		handle._removed = true
		if detach then
			local d = detach
			detach = nil
			pcall(d)
		end
		rollback_action()
	end
	if in_config() then
		table.insert(active_config_actions, handle)
	elseif not explicit_source and km_host and km_host.attach_disposer then
		detach = km_host.attach_disposer(function()
			detach = nil
			pcall(handle.remove)
		end)
	end
	return handle
end

-- Called by the core (and public `mare.keymap.invoke`) for a binding whose action isn't a core
-- one: true = took the key, false = declined, nil = no such Lua action.
local function invoke_action(id, ev)
	local fn = fns[id]
	if not fn then
		local a = named(id)
		fn = a and a.fn
	end
	if not fn then
		return nil
	end
	local ok, r = pcall(fn, ev)
	if not ok then
		if ui and ui.notify then
			ui.notify("keymap: " .. tostring(r), "error")
		end
		return true
	end
	return r ~= false
end

--- invoke(action, opts?) -> boolean|nil: run a registered Lua action (named or `fn:N`).
--- `opts` may provide `{ key?, context? }`; `opts.key` is normalized before passing to the handler.
--- Returns `true` if handled, `false` if declined, or `nil` if no such Lua action exists.
function mare.keymap.invoke(action, opts)
	if type(action) ~= "string" then
		error("mare.keymap.invoke: action must be a string", 2)
	end
	if opts ~= nil and type(opts) ~= "table" then
		error("mare.keymap.invoke: opts must be a table", 2)
	end
	local ev = {}
	if opts then
		if opts.key ~= nil then
			ev.key = normalize_seq(tostring(opts.key))
		end
		if opts.context ~= nil then
			ev.context = tostring(opts.context)
		end
	end
	return invoke_action(action, ev)
end

--- resolve(context, seq, opts?) -> { key, masked, candidates }: resolve a key sequence along
--- `context`'s lookup chain under optional predicate flags (`{ editor_empty?, working?,
--- completion_open?, paste_burst? }`).
function mare.keymap.resolve(context, seq, opts)
	if type(context) ~= "string" then
		error("mare.keymap.resolve: context must be a string", 2)
	end
	if type(seq) ~= "string" then
		error("mare.keymap.resolve: seq must be a string", 2)
	end
	if opts ~= nil and type(opts) ~= "table" then
		error("mare.keymap.resolve: opts must be a table", 2)
	end
	return km_host.resolve(context, seq, opts)
end

--- is_reserved(key) -> boolean: whether `key` (or any token in a sequence) is reserved by the core.
function mare.keymap.is_reserved(key)
	if type(key) ~= "string" then
		error("mare.keymap.is_reserved: key must be a string", 2)
	end
	local norm = normalize_seq(key)
	for tok in norm:gmatch("%S+") do
		if km_host.is_reserved(tok) then
			return true
		end
	end
	return km_host.is_reserved(norm)
end

--- actions() -> { {name, desc, contexts?, core, essential?} }: core actions from the registry followed by
--- registered Lua actions (`core = false`).
function mare.keymap.actions()
	local out = {}
	for _, a in ipairs(km_host.core_actions()) do
		out[#out + 1] = {
			name = a.name,
			desc = a.desc,
			contexts = a.contexts,
			core = true,
			essential = a.essential == true,
			declines = a.declines == true,
		}
	end
	local names = {}
	for name in pairs(actions) do
		names[#names + 1] = name
	end
	table.sort(names)
	for _, name in ipairs(names) do
		local a = named(name)
		if a then
			out[#out + 1] = { name = name, desc = a.desc, contexts = a.contexts, core = false, essential = false }
		end
	end
	return out
end

--- list(opts?) -> bindings[] | snapshot:
--- Without `opts.snapshot = true`, returns every reachable unmasked binding (optionally filtered
--- to `opts.context`). With `opts.snapshot = true`, returns the full keymap snapshot
--- `{ contexts, actions, bindings, conflicts }` (optionally filtered to `opts.context`'s chain).
function mare.keymap.list(opts)
	if opts ~= nil then
		if type(opts) ~= "table" then
			error("mare.keymap.list: opts must be a table", 2)
		end
		if opts.context ~= nil and type(opts.context) ~= "string" then
			error("mare.keymap.list: opts.context must be a string", 2)
		end
		if opts.snapshot == true then
			return km_host.snapshot(opts.context)
		end
	end
	local filter_context = opts and opts.context or nil
	local out = {}
	for k, st in pairs(maps) do
		local blocked = {}
		for i = #st, 1, -1 do
			local m = st[i]
			local c = m.context
			local cov = blocked[c] or 0
			local sm = state_mask(m._all or 0, m._none or 0)
			if m.action == false then
				blocked[c] = cov | sm
			elseif (sm & (~cov & 0xFFFF)) ~= 0 then
				if not filter_context or c == filter_context then
					out[#out + 1] = {
						key = k,
						context = c,
						desc = m.desc or (named(m.action) or {}).desc or core_desc(m.action) or m.action,
						action = m.action,
						when = m.when,
						layer = m.layer or layer_name(m.rank),
						priority = m.priority or 0,
						owner = m.owner,
						_rank = m.rank or 1,
						_seq = m.seq or 0,
					}
				end
				local declines = fns[m.action] ~= nil or actions[m.action] ~= nil or core_declines(m.action)
				if not declines then
					blocked[c] = cov | sm
				end
			end
		end
	end
	table.sort(out, function(a, b)
		if a.key ~= b.key then
			return a.key < b.key
		end
		if a.context ~= b.context then
			return a.context < b.context
		end
		if a._rank ~= b._rank then
			return a._rank > b._rank
		end
		if (a.priority or 0) ~= (b.priority or 0) then
			return (a.priority or 0) > (b.priority or 0)
		end
		return a._seq > b._seq
	end)
	for _, row in ipairs(out) do
		row._rank = nil
		row._seq = nil
	end
	return out
end

--- hint(action, context?) -> { "ctrl+t", … }: the keys that run `action` with focus at `context`
--- (default "editor"), ordered by source layer (project > user > plugin > default), priority, then binding order.
function mare.keymap.hint(action, context)
	return km_host.hint(action, context or "editor")
end

-- Register internal engine hooks on the per-VM registry-held `core.keymap` handle.
if km_host then
	km_host.begin_reload = begin_reload
	km_host.rollback_reload = rollback_reload
	km_host.finish_reload = finish_reload
	km_host.settle = settle
	km_host.set_settled = function(v)
		settled = v == true
	end
	km_host.is_settled = function()
		return settled
	end
	km_host.invoke = invoke_action
	km_host.maps = function()
		return maps
	end
	km_host.current_source = source_info
	km_host.config_validate_stage = config_validate_stage
	km_host.config_snapshot = config_snapshot
	km_host.config_restore = config_restore
	km_host.config_rollback_contexts = config_rollback_contexts
	km_host.config_commit_contexts = config_commit_contexts
	km_host.config_actions_snapshot = config_actions_snapshot
	km_host.config_actions_restore = config_actions_restore
	km_host.config_actions_commit = config_actions_commit
	km_host.apply_config_keys = apply_config_keys
end
