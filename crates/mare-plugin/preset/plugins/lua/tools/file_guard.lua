-- tools.file_guard: paths, read-before-edit observations, guarded atomic writes and file locks;
-- part of the `tools` plugin (plugins/tools.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local tools = D.registry.tools

	---------------------------------------------------------------------------
	-- built-in tools
	---------------------------------------------------------------------------

	local function abs(p)
		return mare.fs.resolve(p)
	end
	local rel = require("mare_lib").rel
	local function short(p)
		return rel(abs(p or ""))
	end

	---------------------------------------------------------------------------
	-- "read before edit" ( fs-observation-policy; on by default like
	-- before editing). `read` records the version it saw (size, mtime, content hash); edit,
	-- apply_patch and overwriting write need that record and refuse when the file changed
	-- since; the version is compared again right before writing; new files are created
	-- without clobbering one created meanwhile. Records are per session and not persisted.
	-- Extension points: `fs/observed` (event), `fs/edit-intent` / `fs/write-intent` (policy
	-- events: add checks), tools:observe / tools:observation, `read_before_edit = false`.
	---------------------------------------------------------------------------
	local observed = {} -- abs path -> {kind = "present", size, mtime, hash} | {kind = "absent"}
	ctx:on("session/changed", function()
		observed = {}
	end)

	--- version_of(path, data?, max_bytes?) -> version, data | nil, nil, err (err ends in
	--- "(too_large)" when the file is over max_bytes)
	local function version_of(path, data, max_bytes)
		local st = mare.fs.stat(path)
		if not st or st.kind ~= "file" then
			return nil
		end
		local err
		if not data then
			data, err = mare.fs.read(path, max_bytes and { max_bytes = max_bytes } or nil)
		end
		if not data then
			return nil, nil, err
		end
		return { kind = "present", size = #data, mtime = st.mtime, hash = mare.crypto.sha256(data) }, data
	end

	-- records are keyed by the file's identity (device + inode; Windows volume + index), so a
	-- symlink or another spelling of the same file finds them;
	-- a file that doesn't exist yet is keyed by its resolved path. Computed at record time, so
	-- an atomic replace (new inode) re-keys the record with the edit that caused it.
	local function obs_key(path)
		return mare.fs.identity(path) or ("path:" .. path)
	end
	local function record(path, obs)
		observed[obs_key(path)] = obs
		ctx.bus:emit("fs/observed", { path = path, observation = obs })
	end

	--- observe(path, data?) — record that the agent has seen `path` now (data = its content
	--- if already read; a missing file is recorded as absent)
	function tools.observe(_, _, path, data)
		path = mare.fs.resolve(path)
		record(path, version_of(path, data) or { kind = "absent" })
	end
	--- observation(path) -> the recorded version or nil
	function tools.observation(_, _, path)
		return observed[obs_key(mare.fs.resolve(path))]
	end

	local short_path = rel

	-- version (from guard) -> the bytes its hash was computed from: commit compares those
	-- instead of hashing the file again; weak keys, so it lives as long as the version
	local hashed = setmetatable({}, { __mode = "k" })

	--- guard(path, verb, tool) -> current {version, data} | nil, error message. verb =
	--- "edit" (must exist and be observed) | "write" (create, or overwrite an observed file)
	local function guard(path, verb, tool)
		-- editing loads the whole file: bounded, and only UTF-8 text (`too_large` / `not_text`); a
		-- write that replaces the file needn't understand its old bytes
		local cur, data, rerr = version_of(path, nil, verb == "edit" and cfg.edit_max_bytes or nil)
		if cur then
			hashed[cur] = data
		end
		if rerr and string.find(tostring(rerr), "(too_large)", 1, true) then
			return nil,
				("%s %s: %s"):format(tool, short_path(path), (string.gsub(tostring(rerr), "^cannot read [^:]*: ", "")))
		end
		if verb == "edit" and data and not mare.text.is_utf8(data) then
			return nil,
				("%s %s: invalid UTF-8 text — %s only edits UTF-8 text files (not_text)"):format(
					tool,
					short_path(path),
					tool
				)
		end
		if cfg.read_before_edit then
			local prior = observed[obs_key(path)]
			if verb == "edit" or cur then
				if not prior then
					return nil,
						("%s %s: read it first — %s refuses files not read in this session (not_observed)"):format(
							tool,
							short_path(path),
							tool
						)
				end
				if not cur then
					return nil, ("%s %s: file not found"):format(tool, short_path(path))
				end
				if prior.kind ~= "present" or prior.hash ~= cur.hash then
					return nil,
						("%s %s: the file changed since you read it — read it again before editing (stale)"):format(
							tool,
							short_path(path)
						)
				end
			end
		end
		local ev = verb == "edit" and "fs/edit-intent" or "fs/write-intent"
		local v = D.policy:decide(
			ev,
			table.pack({ path = path, tool = tool, exists = cur ~= nil, observation = observed[obs_key(path)] })
		)
		if v.decision ~= "allow" then
			return nil, ("%s %s: %s"):format(tool, short_path(path), v.reason or "refused by policy")
		end
		return { version = cur, data = data }
	end

	--- commit(path, content, cur, tool) — atomic write; re-checks the version right before
	--- writing, creates new files without clobbering, then records the new version
	local function commit(path, content, cur, tool, opts)
		if cur.version then
			local st = mare.fs.stat(path)
			local stale = not st or st.size ~= cur.version.size or st.mtime ~= cur.version.mtime
			if not stale then
				-- same size and mtime isn't the same content (a same-size rewrite with the
				-- mtime restored): compare the content too — against the bytes
				-- the guard hashed when it has them (equal bytes = equal hash, without hashing
				-- again), else by hash
				local now, seen = mare.fs.read(path), hashed[cur.version]
				if not now then
					stale = true
				elseif seen then
					stale = now ~= seen
				else
					stale = mare.crypto.sha256(now) ~= cur.version.hash
				end
			end
			if stale then
				return nil,
					("%s %s: the file changed while editing — read it again (stale)"):format(tool, short_path(path))
			end
		end
		-- write through a symlink to its target: the atomic rename would otherwise replace the
		-- link itself with a regular file and leave the real file unchanged
		local target = (cur.version and mare.fs.identity(path)) and mare.fs.realpath(path) or path
		-- a file with other hard links is rewritten in place: an atomic rename would give this
		-- name a new file and leave the other links with the old content
		local st = cur.version and mare.fs.stat(target)
		local linked = st and (st.nlink or 1) > 1
		local ok, err = mare.fs.write(
			target,
			content,
			{ atomic = not linked, create_new = cur.version == nil, mode = opts and opts.mode or nil }
		)
		if not ok then
			if not cur.version and mare.fs.exists(path) then
				return nil,
					("%s %s: the file was created by someone else meanwhile — read it first"):format(
						tool,
						short_path(path)
					)
			end
			return nil, ("%s %s: %s"):format(tool, short_path(path), tostring(err))
		end
		record(path, (version_of(path, content)))
		return true
	end
	--- Waiter-queue per-file lock table (keyed by realpath + identity, sorted to avoid deadlocks).
	--- Used for both `tools:with_file_lock` and the outer pipeline lock (`acquire_pipeline_lock`).
	--- Strict lock order is always `pipeline_locks` -> `file_locks`, which works cleanly across
	--- `mare.exec.run` fresh threads and cross-plugin service calls such as `apply_patch`.
	local function make_lock_table()
		local locks = {} -- key -> {locked, waiters}
		local function acquire(paths, cancel)
			local keys, seen = {}, {}
			for _, p in ipairs(paths) do
				-- always the resolved path: an atomic write renames a new file over the old one, so
				-- the identity changes under a waiting caller and can't be the only key; plus the
				-- identity when the file exists, so two hard-linked names still share a lock
				-- (the canonical directory plus the name: the same before and after the file exists,
				-- which a canonicalized file path is not)
				local id = mare.fs.identity(p)
				local abs = mare.fs.resolve(p)
				local dir, name = abs:match("^(.*)[/\\]([^/\\]+)$")
				local real = dir and dir ~= "" and (mare.fs.realpath(dir) .. "/" .. name) or mare.fs.realpath(abs)
				local ks = { "path:" .. real }
				if id then
					ks[2] = "id:" .. tostring(id)
				end
				for _, k in ipairs(ks) do
					if not seen[k] then
						seen[k] = true
						keys[#keys + 1] = k
					end
				end
			end
			table.sort(keys)
			local acquired = {}
			local released = false
			local function release_acquired()
				if released then
					return
				end
				released = true
				for i = #acquired, 1, -1 do
					local entry = acquired[i]
					local l = entry.lock
					local next_waiter
					while #l.waiters > 0 do
						local w = table.remove(l.waiters, 1)
						if not w.cancelled then
							next_waiter = w
							break
						end
					end
					if next_waiter then
						next_waiter.granted = true
						next_waiter.tx:send(true)
					else
						l.locked = false
						if locks[entry.key] == l then
							locks[entry.key] = nil
						end
					end
				end
			end
			local committed = false
			local pending_waiter, pending_key, pending_lock
			local _abort_guard <close> = setmetatable({}, {
				__close = function()
					if committed then
						return
					end
					if pending_waiter then
						if pending_waiter.granted then
							acquired[#acquired + 1] = { key = pending_key, lock = pending_lock }
						else
							pending_waiter.cancelled = true
						end
						pending_waiter, pending_key, pending_lock = nil, nil, nil
					end
					release_acquired()
				end,
			})
			for _, k in ipairs(keys) do
				if cancel and cancel:is_cancelled() then
					release_acquired()
					return nil, "cancelled"
				end
				local l = locks[k]
				if not l then
					l = { locked = true, waiters = {} }
					locks[k] = l
				else
					local tx, rx = mare.task.channel()
					local waiter = { tx = tx, granted = false, cancelled = false }
					pending_waiter, pending_key, pending_lock = waiter, k, l
					l.waiters[#l.waiters + 1] = waiter
					local ok_wait, wait_err
					if cancel then
						local ok_sel, which = pcall(mare.task.select, function()
							return rx:recv()
						end, function()
							cancel:wait()
						end)
						if not ok_sel then
							ok_wait, wait_err = false, which
						elseif which ~= 1 then
							ok_wait, wait_err = false, "cancelled"
						else
							ok_wait = true
						end
					else
						ok_wait, wait_err = pcall(rx.recv, rx)
					end
					if not ok_wait then
						if waiter.granted then
							-- Ownership was handed off concurrently with cancellation; pass it along.
							acquired[#acquired + 1] = { key = k, lock = l }
						else
							waiter.cancelled = true
						end
						pending_waiter, pending_key, pending_lock = nil, nil, nil
						release_acquired()
						return nil, wait_err
					end
					pending_waiter, pending_key, pending_lock = nil, nil, nil
				end
				acquired[#acquired + 1] = { key = k, lock = l }
			end
			committed = true
			return release_acquired
		end
		return locks, acquire
	end
	local file_locks, acquire_file_lock = make_lock_table()
	local _, acquire_pipeline_lock = make_lock_table()
	--- with_file_lock(paths, fn) -> fn() — mutations of the same file run one at a time
	--- (keyed by realpath + identity; locks taken in sorted order, so two calls needing
	--- overlapping sets can't deadlock); different files still run in parallel
	function tools.with_file_lock(_, _, paths, fn)
		local release, err = acquire_file_lock(paths)
		if not release then
			error(err, 0)
		end
		local _guard <close> = setmetatable({}, { __close = release })
		return fn()
	end

	--- fs_guard(path, verb, tool) -> cur | nil, err — the read-before-edit check other edit
	--- tools (apply_patch, plugins) use; verb = "edit" | "write"
	function tools.fs_guard(_, _, path, verb, tool)
		return guard(mare.fs.resolve(path), verb, tool)
	end
	--- fs_commit(path, content, cur, tool, opts?) -> true | nil, err — atomic, version-checked
	--- write; opts.mode sets the file mode (a moved file keeps its source's mode)
	function tools.fs_commit(_, _, path, content, cur, tool, opts)
		return commit(mare.fs.resolve(path), content, cur, tool, opts)
	end

	return {
		abs = abs,
		rel = rel,
		short = short,
		record = record,
		version_of = version_of,
		guard = guard,
		commit = commit,
		acquire_pipeline_lock = acquire_pipeline_lock,
		lock_count = function()
			local n = 0
			for _ in pairs(file_locks) do
				n = n + 1
			end
			return n
		end,
	}
end
