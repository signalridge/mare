-- session.queries: the read-only `session` service methods (info, availability, entries,
-- leaf, ids, the session list);
-- part of the `session` plugin (plugins/session.lua), loaded at the start of its apply.
return function(D)
	local ctx, S, disabled, dir, session = D.ctx, D.S, D.disabled, D.dir, D.session
	local function path_to(...)
		return D.history.path_to(...)
	end
	local function ensure_full(...)
		return D.history.ensure_full(...)
	end

	-- A page token identifies one authorized view, not an authorization grant. Callers must
	-- assemble and filter the current view before invoking this helper on every request.
	function D.page(method, opts, rows, identity, extra_generation)
		local lib = require("mare_lib")
		local limit = opts.limit
		if limit == nil then
			limit = 20
		end
		if type(limit) ~= "number" or limit < 1 or limit % 1 ~= 0 then
			lib.fail("invalid_arguments", "limit must be a positive integer")
		end
		local caller_owner = (ctx.kernel and ctx.kernel.get_caller and ctx.kernel:get_caller()) or "host"
		local binding = mare.crypto.sha256(mare.json.encode_strict({
			method = method,
			owner = caller_owner,
			identity = identity,
		}))
		local row_hashes = {}
		for _, row in ipairs(rows) do
			local ok_enc, encoded = pcall(mare.json.encode_strict, row)
			if not ok_enc then
				encoded = mare.json.encode(row)
			end
			row_hashes[#row_hashes + 1] = mare.crypto.sha256(encoded)
		end
		local generation = mare.crypto.sha256(mare.json.encode_strict({
			session_id = S.id or false,
			generation = S.generation or false,
			epoch = ctx.kernel:session_epoch(),
			leaf_entry_id = S.leaf or false,
			rows = mare.crypto.sha256(table.concat(row_hashes)),
			extra = extra_generation or false,
		}))
		local offset = 0
		if opts.cursor ~= nil then
			if type(opts.cursor) ~= "string" or #opts.cursor > 4096 then
				lib.fail("invalid_arguments", "invalid session cursor")
			end
			local ok, raw = pcall(mare.crypto.base64url_decode, opts.cursor)
			local decoded, token = false, nil
			if ok and type(raw) == "string" then
				decoded, token = pcall(mare.json.decode_lossless, raw)
			end
			if
				not decoded
				or type(token) ~= "table"
				or token.version ~= 1
				or type(token.method) ~= "string"
				or type(token.binding) ~= "string"
				or type(token.generation) ~= "string"
				or type(token.offset) ~= "number"
				or token.offset < 1
				or token.offset % 1 ~= 0
			then
				lib.fail("invalid_arguments", "invalid session cursor")
			end
			if token.method ~= method or token.binding ~= binding or token.generation ~= generation then
				lib.fail("stale_session", "session view changed; restart pagination")
			end
			if token.offset >= #rows then
				lib.fail("invalid_arguments", "invalid session cursor position")
			end
			offset = token.offset
		end
		local data = mare.json.array({})
		local last = offset + math.min(limit, #rows - offset)
		for i = offset + 1, last do
			data[#data + 1] = rows[i]
		end
		local result = { data = data }
		if last < #rows then
			result.next_cursor = mare.crypto.base64url_encode(mare.json.encode_strict({
				version = 1,
				method = method,
				binding = binding,
				generation = generation,
				offset = last,
			}))
		end
		return result
	end

	function session.info(_, _)
		-- ephemeral: nothing is persisted, no crash recovery
		return {
			session_id = S.id,
			session_epoch = D.store.current_epoch(),
			path = S.path or D.recorded_path,
			-- where the file is (or will be, from the first message on): fixed per session,
			-- so files kept next to it (snapshot journal) can be placed before it exists
			planned = (not disabled or D.recorded_path) and (S.path or D.recorded_path or S.planned) or nil,
			children_dir = (not disabled or D.recorded_path) and D.child_dir() or nil,
			name = S.name,
			leaf_entry_id = S.leaf,
			count = #S.list,
			ephemeral = (mare.args and mare.args.no_session) and true or nil,
			readonly = S.readonly or nil,
			-- read-only view of a file whose end still moved on the re-read
			unstable = S.unstable or nil,
			-- the run that last wrote this (resumed) session ended without a clean close
			crashed = S.crashed,
			-- why -c / --resume found nothing to resume
			resume_error = S.resume_error,
			resume_error_code = S.resume_error_code,
		}
	end
	--- available() -> true | false, why: whether the session store can be used now (the
	--- commands' "session" availability). Not under --no-session (nothing is persisted or
	--- resumed), not in a child agent (its sub-session belongs to the parent run)
	--- and not while session writes are failing (switching would drop the records still
	--- waiting to be written)
	function session.available(_, _)
		if mare.mode == "child" then
			return false, "in a child agent"
		end
		if disabled then
			return false, "without a saved session (--no-session)"
		end
		local ok, err = pcall(ctx.agent.persist_error, ctx.agent)
		-- An ordinary in-flight write during an active turn is not a write failure; session
		-- switches still check `agent:persist_error()` directly inside `require_idle()`.
		if ok and err and err ~= "session persistence in progress" then
			return false, "while session writes are failing"
		end
		return true
	end
	function session.entries(_, _, opts)
		if not (type(opts) == "table" and opts.expand == false) then
			ensure_full()
		end
		return S.list
	end
	function session.list_items(_, _, opts)
		local lib = require("mare_lib")
		if opts ~= nil and type(opts) ~= "table" then
			lib.fail("invalid_arguments", "session.list_items options must be a table")
		end
		opts = opts or {}
		local branch = opts.branch or "all"
		if branch ~= "current" and branch ~= "all" then
			lib.fail("invalid_arguments", "branch must be `current` or `all`")
		end
		local direction = opts.direction or "asc"
		if direction ~= "asc" and direction ~= "desc" then
			lib.fail("invalid_arguments", "direction must be `asc` or `desc`")
		end
		local types_set
		if opts.types ~= nil then
			if type(opts.types) ~= "table" then
				lib.fail("invalid_arguments", "types must be an array of strings")
			end
			types_set = {}
			for _, t in ipairs(opts.types) do
				if type(t) ~= "string" or t == "" then
					lib.fail("invalid_arguments", "types entries must be non-empty strings")
				end
				types_set[t] = true
			end
		end
		ensure_full()
		local source = branch == "current" and (S.leaf and path_to(S.leaf) or {}) or S.list
		local entries = mare.projection.project(source, { preserve_invalid = true })
		local items = mare.store.project_items(mare.json.array(entries))
		local out = {}
		local n = #items
		local start_i, stop_i, step_i = 1, n, 1
		if direction == "desc" then
			start_i, stop_i, step_i = n, 1, -1
		end
		for i = start_i, stop_i, step_i do
			local item = items[i]
			if not types_set or types_set[item.type] then
				out[#out + 1] = item
			end
		end
		local types = false
		if types_set then
			types = mare.json.array({})
			for kind in pairs(types_set) do
				types[#types + 1] = kind
			end
			table.sort(types)
		end
		local res = D.page("session.list_items", opts, out, { branch = branch, types = types, direction = direction })
		if type(S.leaf) == "string" and S.leaf ~= "" then
			res.leaf_entry_id = S.leaf
		end
		return res
	end
	function session.path_to(_, _, leaf)
		ensure_full()
		return path_to(leaf or S.leaf)
	end
	function session.leaf(_, _)
		return S.leaf
	end
	function session.entry_id(_, _, msg)
		return S.ids[msg]
	end
	function D.session_snapshot(opts)
		local lib = require("mare_lib")
		local created_ms = lib.to_epoch_ms(tonumber(opts.created_at_ms)) or 0
		local updated_ms = lib.to_epoch_ms(tonumber(opts.updated_at_ms)) or created_ms
		local snap = {
			session_id = opts.session_id,
			cwd = opts.cwd,
			loaded = opts.loaded == true,
			archived = opts.archived == true,
			created_at_ms = created_ms,
			updated_at_ms = updated_ms,
		}
		if snap.loaded and type(opts.session_epoch) == "string" and opts.session_epoch ~= "" then
			snap.session_epoch = opts.session_epoch
		end
		if type(opts.name) == "string" then
			snap.name = opts.name
		end
		if opts.name_revision ~= nil then
			snap.name_revision = math.floor(tonumber(opts.name_revision) or 0)
		end
		if type(opts.path) == "string" and opts.path ~= "" then
			snap.path = opts.path
		end
		if type(opts.parent_session_id) == "string" and opts.parent_session_id ~= "" then
			snap.parent_session_id = opts.parent_session_id
		end
		if type(opts.source_leaf_entry_id) == "string" and opts.source_leaf_entry_id ~= "" then
			snap.source_leaf_entry_id = opts.source_leaf_entry_id
		end
		if type(opts.leaf_entry_id) == "string" and opts.leaf_entry_id ~= "" then
			snap.leaf_entry_id = opts.leaf_entry_id
		end
		if opts.status == "idle" or opts.status == "busy" then
			snap.status = opts.status
		end
		if opts.ephemeral ~= nil then
			snap.ephemeral = opts.ephemeral == true
		end
		return snap
	end

	function D.normalize_cwd(p)
		local r = mare.fs.resolve(p)
		return (#r > 1 and r:gsub("[/\\]+$", "")) or r
	end

	function D.current_session_snapshot()
		local archived = false
		local idx_updated
		if S.path then
			local ok, rows = pcall(mare.store.index_read, { scope = "all", archived = "any", session_id = S.id })
			if ok and type(rows) == "table" and rows[1] then
				archived = rows[1].archived == true
				idx_updated = rows[1].updated_at_ms
			end
		end
		local parent_session_id = S.parent_session_id
		if not parent_session_id and D.recorded_path then
			local ok_rc, rc = pcall(mare.store.read_from_compaction, D.recorded_path)
			if
				ok_rc
				and type(rc) == "table"
				and type(rc.header) == "table"
				and type(rc.header.parent_session_id) == "string"
				and rc.header.parent_session_id ~= ""
			then
				parent_session_id = rc.header.parent_session_id
				S.parent_session_id = parent_session_id
			end
		end
		local created = tonumber(S.created_at_ms) or mare.task.now()
		return D.session_snapshot({
			session_id = S.id,
			session_epoch = (ctx.kernel and ctx.kernel:session_epoch()) or nil,
			name = S.name,
			name_revision = S.name_revision or 0,
			cwd = S.cwd or mare.cwd,
			path = S.path or D.recorded_path or ((not disabled) and S.planned) or nil,
			parent_session_id = parent_session_id,
			leaf_entry_id = S.leaf,
			loaded = true,
			status = (ctx.agent and ctx.agent:activity().busy) and "busy" or "idle",
			ephemeral = disabled == true,
			archived = archived,
			created_at_ms = created,
			updated_at_ms = tonumber(idx_updated) or math.max(created, mare.task.now()),
		})
	end

	local function loaded_sessions_info()
		local host_loaded = ctx.kernel and ctx.kernel.list_sessions and ctx.kernel:list_sessions() or nil
		local loaded_map, loaded_epochs, loaded_snapshot = {}, {}, {}
		if type(host_loaded) == "table" then
			for _, item in ipairs(host_loaded) do
				if type(item) == "table" and type(item.session_id) == "string" and item.session_id ~= "" then
					loaded_map[item.session_id] = true
					if type(item.session_epoch) == "string" and item.session_epoch ~= "" then
						loaded_epochs[item.session_id] = item.session_epoch
					end
					loaded_snapshot[#loaded_snapshot + 1] = {
						session_id = item.session_id,
						session_epoch = item.session_epoch or false,
					}
				end
			end
			table.sort(loaded_snapshot, function(a, b)
				return a.session_id < b.session_id
			end)
		elseif type(S.id) == "string" and S.id ~= "" then
			local epoch = (ctx.kernel and ctx.kernel.session_epoch and ctx.kernel:session_epoch()) or nil
			loaded_map[S.id] = true
			loaded_epochs[S.id] = epoch
			loaded_snapshot[1] = { session_id = S.id, session_epoch = epoch or false }
		end
		return loaded_map, loaded_epochs, loaded_snapshot
	end

	function session.get(_, caller, session_id)
		local lib = require("mare_lib")
		if type(session_id) ~= "string" or session_id == "" then
			lib.fail("invalid_arguments", "session_id must be a non-empty string")
		end
		local cross_project = not mare.contracts.is_peer(caller)
		local path_like = session_id:find("[/\\]") or session_id:find("%.jsonl$")
		local allow_path = path_like and mare.contracts.allows_session_path(caller) or false
		local resolved_host_cwd = D.normalize_cwd(mare.cwd)
		local function same_project(cwd)
			return type(cwd) == "string" and cwd ~= "" and D.normalize_cwd(cwd) == resolved_host_cwd
		end
		local cur_path = S.path or D.recorded_path
		if
			session_id == S.id
			or (
				allow_path
				and cur_path
				and (session_id == cur_path or D.normalize_cwd(session_id) == D.normalize_cwd(cur_path))
			)
		then
			if not cross_project and not same_project(S.cwd or mare.cwd) then
				lib.fail("unknown_session", ("no session matches %q"):format(session_id))
			end
			return D.current_session_snapshot()
		end
		if path_like and not allow_path and cross_project then
			lib.fail("denied", "restricted callers cannot address sessions by file path")
		end
		local resolved_query = (path_like and not allow_path) and nil or session_id
		local aok, arows = pcall(mare.store.index_read, { scope = "all", archived = "any", kind = "any" })
		local hits = {}
		local resolved_path = allow_path and mare.fs.exists(session_id) and mare.fs.resolve(session_id) or nil
		if aok and type(arows) == "table" then
			for _, r in ipairs(arows) do
				-- Fail closed for restricted callers: rows without an explicit matching cwd are
				-- excluded before id/prefix matching so cross-project sessions cannot be probed.
				if cross_project or same_project(r.cwd) then
					if resolved_path and (r.path == resolved_path or r.path == session_id) then
						hits = { r }
						break
					elseif resolved_query and r.session_id == resolved_query then
						hits = { r }
						break
					elseif
						resolved_query
						and type(r.session_id) == "string"
						and r.session_id:sub(1, #resolved_query) == resolved_query
					then
						hits[#hits + 1] = r
					end
				end
			end
		end
		if #hits > 1 then
			lib.fail("invalid_arguments", ("session id %q is ambiguous (%d matches)"):format(session_id, #hits))
		end
		local row = hits[1]
		if row then
			if row.session_id == S.id then
				if not cross_project and not same_project(S.cwd or mare.cwd) then
					lib.fail("unknown_session", ("no session matches %q"):format(session_id))
				end
				return D.current_session_snapshot()
			end
			local known_row_cwd = type(row.cwd) == "string" and row.cwd ~= "" and row.cwd or nil
			local hdr = {}
			if type(row.path) == "string" and row.path ~= "" then
				local ok_rc, rc = pcall(mare.store.read_from_compaction, row.path)
				if ok_rc and type(rc) == "table" and type(rc.header) == "table" then
					hdr = rc.header
				end
			end
			local header_cwd = type(hdr.cwd) == "string" and hdr.cwd ~= "" and hdr.cwd or nil
			if not cross_project and not same_project(header_cwd or known_row_cwd) then
				lib.fail("unknown_session", ("no session matches %q"):format(session_id))
			end
			local row_cwd = header_cwd or known_row_cwd or (cross_project and mare.cwd) or nil
			local created = tonumber(hdr.created_at_ms) or tonumber(row.created_at_ms) or 0
			local loaded_map, loaded_epochs = loaded_sessions_info()
			local is_loaded = type(row.session_id) == "string" and loaded_map[row.session_id] == true
			return D.session_snapshot({
				session_id = row.session_id,
				session_epoch = is_loaded and loaded_epochs[row.session_id] or nil,
				name = (type(row.title) == "string" and row.title ~= "" and row.title)
					or (type(hdr.name) == "string" and hdr.name ~= "" and hdr.name)
					or nil,
				cwd = (type(hdr.cwd) == "string" and hdr.cwd ~= "" and hdr.cwd) or row_cwd,
				path = row.path,
				parent_session_id = (
					type(hdr.parent_session_id) == "string"
					and hdr.parent_session_id ~= ""
					and hdr.parent_session_id
				)
					or (type(row.parent_session_id) == "string" and row.parent_session_id ~= "" and row.parent_session_id)
					or nil,
				loaded = is_loaded,
				archived = row.archived == true,
				created_at_ms = created,
				updated_at_ms = tonumber(row.updated_at_ms) or created,
			})
		end
		if resolved_path then
			local ok_rc, rc = pcall(mare.store.read_from_compaction, resolved_path)
			local hdr = (ok_rc and type(rc) == "table" and type(rc.header) == "table") and rc.header or nil
			if hdr and type(hdr.session_id) == "string" and hdr.session_id ~= "" then
				local st = mare.fs.stat(resolved_path)
				local created = tonumber(hdr.created_at_ms) or (st and tonumber(st.mtime)) or 0
				local loaded_map, loaded_epochs = loaded_sessions_info()
				local is_loaded = loaded_map[hdr.session_id] == true
				return D.session_snapshot({
					session_id = hdr.session_id,
					session_epoch = is_loaded and loaded_epochs[hdr.session_id] or nil,
					name = type(hdr.name) == "string" and hdr.name ~= "" and hdr.name or nil,
					cwd = (type(hdr.cwd) == "string" and hdr.cwd ~= "" and hdr.cwd) or mare.cwd,
					path = resolved_path,
					parent_session_id = type(hdr.parent_session_id) == "string"
							and hdr.parent_session_id ~= ""
							and hdr.parent_session_id
						or nil,
					loaded = is_loaded,
					archived = false,
					created_at_ms = created,
					updated_at_ms = (st and tonumber(st.mtime)) or created,
				})
			end
		end
		lib.fail("unknown_session", ("no session matches %q"):format(session_id))
	end

	function session.list(_, caller, opts)
		local lib = require("mare_lib")
		if opts ~= nil and type(opts) ~= "table" then
			lib.fail("invalid_arguments", "session.list options must be a table")
		end
		opts = opts or {}
		local all = opts.all == true
		local archived = opts.archived == true
		local cwd_filter = opts.cwd
		if cwd_filter ~= nil and (type(cwd_filter) ~= "string" or cwd_filter == "") then
			lib.fail("invalid_arguments", "cwd must be a non-empty string")
		end
		local parent_filter = opts.parent_session_id
		if parent_filter ~= nil and (type(parent_filter) ~= "string" or parent_filter == "") then
			lib.fail("invalid_arguments", "parent_session_id must be a non-empty string")
		end
		local loaded_filter = opts.loaded
		if loaded_filter ~= nil and type(loaded_filter) ~= "boolean" then
			lib.fail("invalid_arguments", "loaded must be a boolean")
		end
		local unrestricted = not mare.contracts.is_peer(caller)
		local normalize_cwd = D.normalize_cwd
		local resolved_host_cwd = normalize_cwd(mare.cwd)
		local resolved_cwd_filter = cwd_filter and normalize_cwd(cwd_filter) or nil
		if not unrestricted then
			if all then
				lib.fail("denied", "restricted callers cannot list sessions across projects")
			end
			if
				resolved_cwd_filter ~= nil
				and (mare.store.dir(resolved_cwd_filter) ~= dir or resolved_cwd_filter ~= resolved_host_cwd)
			then
				lib.fail("denied", "restricted callers cannot list sessions outside the current project")
			end
		end
		local warnings = {}
		local function listed(path)
			local rows, unsupported = mare.store.list(path)
			for _, p in ipairs(unsupported or {}) do
				warnings[#warnings + 1] = p
			end
			return rows
		end
		local function announce()
			if #warnings > 0 then
				D.warn_unsupported(warnings)
			end
		end
		local out
		if resolved_cwd_filter and not all then
			out = listed(mare.store.dir(resolved_cwd_filter))
		elseif all then
			out = {}
			for _, d in ipairs(mare.fs.list(mare.store.root) or {}) do
				if d.kind == "dir" then
					for _, s in ipairs(listed(mare.store.root .. "/" .. d.name)) do
						out[#out + 1] = s
					end
				end
			end
			table.sort(out, function(a, b)
				return a.mtime > b.mtime
			end)
		else
			out = listed(dir)
		end
		local idx_by_id, idx_by_path = {}, {}
		local aok, arows = pcall(mare.store.index_read, { scope = "all", archived = "any", kind = "any" })
		if aok and type(arows) == "table" then
			for _, r in ipairs(arows) do
				if type(r.session_id) == "string" and r.session_id ~= "" then
					idx_by_id[r.session_id] = r
				end
				if type(r.path) == "string" and r.path ~= "" then
					idx_by_path[r.path] = r
				end
			end
		end
		local loaded_map, loaded_epochs, loaded_snapshot = loaded_sessions_info()
		local filtered = {}
		for _, row in ipairs(out) do
			local hdr = type(row.header) == "table" and row.header or {}
			local idx = (type(hdr.session_id) == "string" and idx_by_id[hdr.session_id])
				or (type(row.path) == "string" and idx_by_path[row.path])
			local sid = (type(hdr.session_id) == "string" and hdr.session_id ~= "" and hdr.session_id)
				or (idx and idx.session_id)
				or ""
			local is_local = S.id ~= nil and sid ~= "" and sid == S.id
			local is_loaded = sid ~= "" and loaded_map[sid] == true
			local is_arch = (idx and idx.archived == true) or false
			local parent_id = (
				type(hdr.parent_session_id) == "string"
				and hdr.parent_session_id ~= ""
				and hdr.parent_session_id
			)
				or (idx and type(idx.parent_session_id) == "string" and idx.parent_session_id ~= "" and idx.parent_session_id)
				or nil
			local known_row_cwd = (type(hdr.cwd) == "string" and hdr.cwd ~= "" and hdr.cwd)
				or (idx and type(idx.cwd) == "string" and idx.cwd ~= "" and idx.cwd)
				or nil
			local row_cwd = known_row_cwd or (unrestricted and (cwd_filter or mare.cwd)) or nil
			if
				is_arch == archived
				and (parent_filter == nil or parent_id == parent_filter)
				and (loaded_filter == nil or is_loaded == loaded_filter)
				and (unrestricted or (row_cwd ~= nil and normalize_cwd(row_cwd) == resolved_host_cwd))
				and (not resolved_cwd_filter or (row_cwd ~= nil and normalize_cwd(row_cwd) == resolved_cwd_filter))
			then
				local name = (is_local and S.name)
					or (idx and type(idx.title) == "string" and idx.title ~= "" and idx.title)
					or (type(hdr.name) == "string" and hdr.name ~= "" and hdr.name)
					or nil
				local created = tonumber(hdr.created_at_ms)
					or (idx and tonumber(idx.created_at_ms))
					or tonumber(row.mtime)
					or 0
				local updated = (idx and tonumber(idx.updated_at_ms)) or tonumber(row.mtime) or created
				filtered[#filtered + 1] = D.session_snapshot({
					session_id = sid,
					session_epoch = is_loaded and loaded_epochs[sid] or nil,
					name = name,
					name_revision = is_local and (S.name_revision or 0) or nil,
					cwd = row_cwd,
					path = row.path,
					parent_session_id = parent_id,
					leaf_entry_id = is_local and S.leaf or nil,
					loaded = is_loaded,
					status = is_local and ((ctx.agent and ctx.agent:activity().busy) and "busy" or "idle") or nil,
					ephemeral = is_local and (disabled == true) or nil,
					archived = is_arch,
					created_at_ms = created,
					updated_at_ms = updated,
				})
			end
		end
		announce()
		table.sort(filtered, function(a, b)
			if a.updated_at_ms ~= b.updated_at_ms then
				return a.updated_at_ms > b.updated_at_ms
			end
			if a.session_id ~= b.session_id then
				return a.session_id < b.session_id
			end
			return (a.path or "") < (b.path or "")
		end)
		return D.page("session.list", opts, filtered, {
			cwd = resolved_cwd_filter or false,
			archived = archived,
			parent_session_id = parent_filter or false,
			loaded = loaded_filter == nil and "any" or (loaded_filter and "loaded" or "unloaded"),
			all = all,
			unrestricted = unrestricted,
		}, loaded_snapshot)
	end
	function session.live_snapshot(_, _)
		local path = (not disabled) and (S.path or S.planned) or nil
		if not S.id or not path or S.partial then
			return nil
		end
		local entries = {
			{
				type = "session",
				version = mare.store.format_version,
				session_id = S.id,
				cwd = S.cwd or mare.cwd,
				created_at_ms = require("mare_lib").to_epoch_ms(S.created_at_ms or mare.task.now()),
				parent_session_id = S.parent_session_id,
			},
		}
		for _, e in ipairs(S.list) do
			entries[#entries + 1] = table.copy(e)
		end
		return {
			path = path,
			entries = entries,
			session_epoch = ctx.kernel:session_epoch(),
			step_boundary = S.step_boundary and (1 + S.step_boundary) or nil,
		}
	end
	function session.baseline_cut(_, _)
		local cut = {
			list = S.list,
			count = #S.list,
			frozen = nil,
		}
		D._baseline_cuts[cut] = true
		local page_slice = require("mare_lib.baseline_page")
		return {
			parts = {
				{
					name = "entries",
					release = function()
						D._baseline_cuts[cut] = nil
						cut.frozen = nil
					end,
					page = function(cursor, max_bytes, limit)
						return page_slice(cut.frozen or cut.list, cut.count, cursor, max_bytes, limit)
					end,
				},
			},
		}
	end

	local function run_history(store_fn, args)
		local snap = session.live_snapshot(session, ctx)
		local exec_opts = snap and { live_sessions = { snap } } or nil
		return store_fn(args, exec_opts)
	end

	function session.search(_, _, opts)
		local page = run_history(mare.store.search_sessions, opts or {})
		if type(page) == "table" and type(page.data) == "table" then
			for _, hit in ipairs(page.data) do
				hit.event_id = nil
			end
		end
		return page
	end

	function session.search_items(_, _, opts)
		return run_history(mare.store.search_items, opts or {})
	end

	function session.get_item(_, _, opts)
		return run_history(mare.store.read_item, opts or {})
	end

	function session.trace(_, _, session_id)
		return run_history(mare.store.trace_session, { session_id = session_id })
	end

	function session.trace_item(_, _, opts)
		return run_history(mare.store.trace_item, opts or {})
	end
end
