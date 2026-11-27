-- session.history: the tree read back — paths, the model context of a leaf, abandoned
-- branches, applying read entries, the full re-read and the index row;
-- part of the `session` plugin (plugins/session.lua), loaded at the start of its apply.
return function(D)
	local ctx, S, disabled = D.ctx, D.S, D.disabled
	local hydrate = D.store.hydrate
	local maybe_yield = require("mare_lib").maybe_yield

	--- Resolve a semantic item id through the Rust item projection to its owning entry in `source`.
	function D.resolve_item_entry(source, item_id)
		if type(item_id) ~= "string" or item_id == "" or type(source) ~= "table" then
			return nil, nil
		end
		local entries = source.entries
		local list = source.list
		if type(entries) ~= "table" or type(list) ~= "table" then
			return nil, nil
		end
		local projected = mare.projection.project(list, { preserve_invalid = true })
		local items = mare.store.project_items(mare.json.array(projected))
		for _, item in ipairs(items or {}) do
			if type(item) == "table" and item.item_id == item_id then
				local entry_id = item.entry_id
				local entry = type(entry_id) == "string" and entries[entry_id] or nil
				if entry then
					return entry, entry_id
				end
			end
		end
		return nil, nil
	end

	--- Path of entries from root to `leaf`.
	local function path_to(leaf)
		local rev = {}
		local seen = {}
		local cur = leaf and S.entries[leaf]
		while cur do
			if seen[cur] then
				-- a parent cycle (e.g. an id collision in an old 8-hex-char session): stop
				-- here and say so instead of looping or silently cutting the path
				mare.log("warn", "session tree has a parent cycle at entry " .. tostring(cur.entry_id))
				break
			end
			seen[cur] = true
			rev[#rev + 1] = cur
			maybe_yield(#rev)
			cur = cur.parent_entry_id and S.entries[cur.parent_entry_id]
		end
		local out = {}
		for i = #rev, 1, -1 do
			out[#out + 1] = rev[i]
			maybe_yield(#out)
		end
		return out
	end

	local function branch_summary_message(e)
		return {
			role = "user",
			branch_summary = true,
			content = {
				{
					type = "text",
					text = "The user explored a different conversation branch before returning here. Summary of that branch:\n\n<summary>\n"
						.. tostring(e.summary)
						.. "\n</summary>",
				},
			},
			files = e.files,
		}
	end

	--- Model context for a leaf: honours the latest compaction on the path.
	local function context_for(leaf)
		local path = path_to(leaf)
		local start, summary = 1, nil
		for i = #path, 1, -1 do
			if path[i].type == "compaction" then
				summary = path[i]
				-- keep from first_kept_entry_id onward
				start = i + 1
				for j = 1, i do
					if path[j].entry_id == summary.first_kept_entry_id then
						start = j
						break
					end
				end
				break
			end
		end
		local msgs = {}
		if summary then
			-- recent user messages kept verbatim come first, the summary last
			for _, pm in ipairs(summary.preserved or {}) do
				local m = table.copy(pm)
				m.preserved = true
				msgs[#msgs + 1] = m
			end
			msgs[#msgs + 1] = {
				role = "user",
				content = {
					{
						type = "text",
						text = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n"
							.. summary.summary
							.. "\n</summary>",
					},
				},
				compaction = true,
				summary = summary.summary,
				details = summary.details,
				method = summary.method,
				-- provider-native compacted history (remote compaction): providers that
				-- understand it replay these items instead of the text above
				remote = summary.remote,
			}
			-- A subsequent compaction may summarize this checkpoint itself. Keep its log ID so
			-- the new range and recovery footer can refer to the previous compaction entry.
			S.ids[msgs[#msgs]] = summary.entry_id
		end
		for i = start, #path do
			local e = path[i]
			if e.type == "message" then
				local m = e.message
				local pruned = S.prunes[e.entry_id]
				if pruned then
					m = table.copy(m)
					m.content = pruned
					m.pruned = true
				end
				S.ids[m] = e.entry_id
				msgs[#msgs + 1] = m
			elseif e.type == "branch_summary" then
				local m = branch_summary_message(e)
				S.ids[m] = e.entry_id
				msgs[#msgs + 1] = m
			end
		end
		return msgs
	end

	--- Messages and earlier branch summaries left behind when moving to another leaf.
	local function abandoned_messages(target)
		local keep = {}
		for _, e in ipairs(path_to(target)) do
			keep[e.entry_id] = true
		end
		local messages = {}
		for _, e in ipairs(path_to(S.leaf)) do
			if not keep[e.entry_id] then
				if e.type == "message" then
					messages[#messages + 1] = e.message
				elseif e.type == "branch_summary" then
					messages[#messages + 1] = branch_summary_message(e)
				end
			end
		end
		return messages
	end

	--- Bring the session's index row up to date (what /resume reads; never the session
	--- files). Called after appending: every field comes from the file itself,
	--- the row only records where this process runs. A failure is logged, never fatal:
	--- the next reconcile repairs the row from the file.
	local function index_update()
		if disabled or not S.path or S.readonly then
			return
		end
		local e = { session_id = S.id, path = S.path, cwd = S.cwd or mare.cwd }
		ctx:spawn(function()
			local ok, err = pcall(mare.store.index_append, e)
			if not ok then
				mare.log("warn", "session index not updated: " .. tostring(err))
			end
		end)
	end

	-- Replay branch settings independently of the compacted message window. New
	-- events use false for the root; legacy custom events inherit their observed leaf.
	local function events_for(leaf)
		local on_path, out = {}, {}
		for i, e in ipairs(path_to(leaf)) do
			maybe_yield(i)
			on_path[e.entry_id] = true
		end
		local observed = false
		for i, e in ipairs(S.list) do
			maybe_yield(i)
			local setting = e.type == "model" or e.type == "custom"
			local at = e.at_entry_id
			if at == nil then
				if e.type == "model" then
					at = false
				else
					at = observed
				end
			end
			if setting and (at == false or on_path[at]) then
				local copy = table.copy(e)
				copy.at_entry_id = at
				out[#out + 1] = copy
			end
			if e.type == "leaf" then
				observed = e.target_entry_id or false
			elseif D.store.is_tree_entry(e) and e.entry_id then
				observed = e.entry_id
			end
		end
		return out
	end
	local function settings_events(leaf)
		local out = {}
		for _, e in ipairs(events_for(leaf)) do
			if
				e.type == "model"
				or e.kind == "sandbox/mode"
				or e.kind == "workspace/dirs"
				or e.kind == "working-directory/changed"
				or e.kind == "permission/preset"
				or e.kind == "approval/policy"
			then
				out[#out + 1] = e
			end
		end
		return out
	end
	local function settings_for(leaf)
		local model, sandbox, cwd, workspace_dirs
		for _, e in ipairs(settings_events(leaf)) do
			if e.type == "model" then
				model = e
			elseif e.kind == "sandbox/mode" and e.provenance == "session-host" then
				sandbox = e.data
			elseif e.kind == "workspace/dirs" and e.provenance == "session-host" and type(e.data) == "table" then
				workspace_dirs = e.data.dirs
			elseif e.kind == "working-directory/changed" and type(e.data) == "table" then
				cwd = e.data.to
			end
		end
		return { model = model, sandbox = sandbox, cwd = cwd, workspace_dirs = workspace_dirs }
	end

	local function apply_entries(entries, defer_hydration)
		S.known_ids = S.known_ids or {}
		local consumed = {}
		for _, e in ipairs(S.list) do
			if e.input_id then
				consumed[e.input_id] = true
			end
		end
		for i, e in ipairs(entries) do
			maybe_yield(i)
			if e.input_id then
				consumed[e.input_id] = true
			end
			if not defer_hydration then
				hydrate(e, S.path)
			end
			if e.entry_id then
				S.known_ids[e.entry_id] = true
			end
			if e.type == "session" then
				S.id = e.session_id or S.id
				S.parent_session_id = e.parent_session_id
				if e.created_at_ms ~= nil then
					S.created_at_ms = e.created_at_ms
				end
			elseif e.type == "label" then
				S.labels[e.target_entry_id] = e.label ~= "" and e.label or nil
			elseif e.type == "name" then
				S.name = e.name
				S.name_revision = (S.name_revision or 0) + 1
			elseif e.type == "leaf" then
				S.leaf = e.target_entry_id
			elseif e.type == "custom" and e.kind == "sandbox/mode" then
				if e.provenance == "session-host" then
					S.sandbox_mode = e.data
				end
			elseif e.type == "custom" and e.kind == "workspace/dirs" then
				if e.provenance == "session-host" and type(e.data) == "table" then
					S.workspace_dirs = e.data.dirs
				end
			elseif e.type == "custom" and e.kind == "agent/queue" then
				if e.provenance == "session-host" then
					require("mare_lib").validate_inbox(e.data)
					S.queue = e.data
				end
			elseif e.type == "custom" and e.kind == "session/operation" then
				require("session.operation_shape")(e.data)
				if e.data.session_id == S.id then
					S.operations = S.operations or {}
					assert(not S.operations[e.data.request_id], "duplicate session operation receipt")
					S.operations[e.data.request_id] = mare.json.decode(mare.json.encode(e.data))
				end
			elseif e.type == "custom" and e.kind == "agent/request" then
				S.pending_request = e.data
			elseif e.type == "custom" and e.kind == "agent/request-end" then
				S.pending_request = nil
			elseif e.type == "state" then
				-- open / clean-close markers: never part of the tree or copied into a fork
				S.last_state = e.state
				goto next_entry
			elseif e.type == "model" then
				S.model_recs[#S.model_recs + 1] = e
			elseif e.type == "prune" then
				for id, c in pairs(e.target_entry_ids or {}) do
					S.prunes[id] = c
				end
			elseif e.type == "tool_state" and e.tool_call_id then
				S.tool_states[e.tool_call_id] =
					{ state = e.state, side_effect = e.side_effect, idempotent = e.idempotent }
			elseif e.entry_id then
				if D.store.is_tree_entry(e) then
					S.entries[e.entry_id] = e
					if e.type == "compaction" then
						for id, content in pairs(e.projections or {}) do
							S.prunes[id] = content
						end
					end
					S.leaf = e.entry_id
					if e.type == "message" and e.message and e.message.role == "assistant" then
						S.pending_request = nil -- the request got its reply
					end
				end
			end
			table.insert(S.list, e)
			::next_entry::
		end
		if S.queue then
			local q = {
				steer = mare.json.array(),
				follow_up = mare.json.array(),
				notes = mare.json.array(S.queue.notes or {}),
			}
			for _, target in ipairs({ "steer", "follow_up" }) do
				for _, e in ipairs(S.queue[target] or {}) do
					if not consumed[e.input_id] then
						q[target][#q[target] + 1] = e
					end
				end
			end
			S.queue = q
		end
	end

	--- Load the whole file (older history skipped by the tail read) — for /tree, /fork, /undo.
	local function ensure_full()
		if not S.partial or not S.path then
			return
		end
		-- Reads and attachment hydration can yield. Build a detached history and only
		-- install it if the live session and all of its nested state are unchanged.
		local table_revision = require("mare_lib.table_revision")
		local revision = table_revision.snapshot(S)
		local leaf = S.leaf
		local source = D.store.new_snapshot()
		source.path = S.path
		local entries
		if S.file then
			entries = mare.store.read_snapshot(S.file)
		elseif S.readonly and not S.lenient then
			entries = mare.store.read_fork_snapshot(S.path, S.id)
		else
			entries = mare.store.read(S.path, { lenient = S.lenient })
		end
		local history = require("session.history")({ ctx = ctx, S = source, disabled = disabled, store = D.store })
		history.apply_entries(entries)
		if not table_revision.unchanged(revision) then
			require("mare_lib").fail("busy", "Cannot expand session history: the active session changed; retry")
		end
		if D.freeze_baseline_cuts then
			D.freeze_baseline_cuts()
		end
		S.entries, S.known_ids, S.list = source.entries, source.known_ids or {}, source.list
		S.labels, S.model_recs, S.prunes, S.tool_states =
			source.labels, source.model_recs, source.prunes, source.tool_states
		S.operations = source.operations or {}
		S.name, S.name_revision = source.name, source.name_revision
		S.leaf = leaf
		S.partial = false
		S.pre_tail_custom = nil
		S.pre_tail_branch_custom = nil
	end

	--- true when the loaded (partial) entries contain the leaf's whole context: walking
	--- parents from the leaf reaches a compaction whose first kept entry is present, without
	--- hitting a missing ancestor
	local function tail_covers_leaf()
		local cur = S.leaf and S.entries[S.leaf]
		local guard = 0
		while cur and guard < 1000000 do
			if cur.type == "compaction" then
				return cur.first_kept_entry_id == nil or S.entries[cur.first_kept_entry_id] ~= nil
			end
			if not cur.parent_entry_id then
				return true -- reached the root
			end
			cur = S.entries[cur.parent_entry_id]
			guard = guard + 1
		end
		return S.leaf == nil
	end

	return {
		path_to = path_to,
		settings_for = settings_for,
		settings_events = settings_events,
		events_for = events_for,
		context_for = context_for,
		abandoned_messages = abandoned_messages,
		index_update = index_update,
		apply_entries = apply_entries,
		ensure_full = ensure_full,
		tail_covers_leaf = tail_covers_leaf,
	}
end
