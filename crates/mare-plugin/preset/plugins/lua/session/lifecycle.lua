-- session.lifecycle: opening a session file (load_file / load_opened), new sessions, the
-- per-session sandbox mode and model restore, then the service is provided;
-- part of the `session` plugin (plugins/session.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg, S, disabled, session = D.ctx, D.cfg, D.S, D.disabled, D.session
	local close_file, reset, changed, add = D.store.close_file, D.store.reset, D.store.changed, D.store.add
	-- Capture the host binding before plugins can replace the public Lua table field. The opaque
	-- PolicyUd snapshot bypasses Lua-table parsing when restoring the session baseline.
	local restore_sandbox_base = mare.proc.sandbox.restore_snapshot
	local restoring_model = false -- resume sets the model: not a new model record
	local warn = D.store.warn
	-- nil alone is a legitimate success for setters; false or nil accompanied by
	-- an error is a reported failure and must not be mistaken for restoration.
	local function critical_call(fn, ...)
		local result = table.pack(fn(...))
		if result[1] == false or (result[1] == nil and result.n > 1 and result[2] ~= nil) then
			error(tostring(result[2] or "restoration refused"), 0)
		end
		return table.unpack(result, 1, result.n)
	end
	local function same_state(a, b)
		if type(a) ~= type(b) then
			return false
		end
		if type(a) ~= "table" then
			return a == b
		end
		for k, v in pairs(a) do
			if not same_state(v, b[k]) then
				return false
			end
		end
		for k in pairs(b) do
			if a[k] == nil then
				return false
			end
		end
		return true
	end
	-- A setter error is recoverable only when the setter left its own observable state unchanged.
	-- Its checked events happen after mutation, so a listener error must retain the poison barrier.
	-- A refused authorization is never recoverable: no fallback may run in its place.
	local function recoverable_llm_call(fn, ...)
		local llm = D.follow.llm()
		local before = llm:get_selection()
		local result = table.pack(pcall(fn, ...))
		local failure
		if not result[1] and type(result[2]) == "table" and result[2].code == "denied" then
			error(result[2], 0)
		end
		if not result[1] then
			failure = result[2] or "setter failed"
		elseif result[2] == false then
			failure = result[3] or "restoration refused"
		elseif result[2] == nil and result.n > 2 and result[3] ~= nil then
			failure = result[3]
		else
			return true, table.unpack(result, 2, result.n)
		end
		if not same_state(before, llm:get_selection()) then
			error(tostring(failure), 0)
		end
		return false, failure
	end
	local function restore_default_model()
		local restored, model = recoverable_llm_call(D.follow.llm().reset_session_model, D.follow.llm())
		if not restored then
			warn("default model not restored: " .. tostring(model))
			return false
		end
		if not model then
			warn("no available default model; configure one before the next turn")
			return false
		end
		return true
	end
	local function restore_default_effort(note)
		local restored, err = recoverable_llm_call(D.follow.llm().reset_session_effort, D.follow.llm())
		if restored then
			warn(note)
		else
			warn(note .. "; default effort not restored: " .. tostring(err))
		end
	end
	-- Sandbox level per session: a switch is written to the session log, applies
	-- from the next call and comes back on resume; a session without one uses the policy that
	-- was current before any session switched (the permissions plugin's default).
	local sandbox_base -- false = unconfined; nil = not captured yet
	local sandbox_base_snapshot
	local function capture_sandbox_base()
		if sandbox_base ~= nil then
			return
		end
		local policy, snapshot, omitted = mare.proc.sandbox.snapshot_current()
		sandbox_base = policy or false
		sandbox_base_snapshot = snapshot or false
		for _, path in ipairs(omitted) do
			warn("sandbox additional writable directory no longer exists: " .. path)
		end
	end
	local function sandbox_apply(mode, dropped_roots, quiet)
		capture_sandbox_base()
		local target = mode or sandbox_base or nil
		local omitted = {}
		if mode and type(mode) == "table" then
			target = {}
			for k, v in pairs(mode) do
				target[k] = v
			end
			for _, key in ipairs({ "writable", "write" }) do
				if type(mode[key]) == "table" then
					local roots = {}
					for _, path in ipairs(mode[key]) do
						if dropped_roots and dropped_roots[path] then
							-- Already revalidated and warned while restoring workspace/dirs.
						elseif type(path) == "string" and mare.proc.sandbox.root_missing(path) then
							omitted[#omitted + 1] = path
						else
							roots[#roots + 1] = path
						end
					end
					target[key] = roots
				end
			end
		end
		local before_generation = mare.proc.sandbox.generation()
		local before_policy = mare.proc.sandbox.current()
		local ok, pol = pcall(critical_call, mare.proc.sandbox.set_current, target)
		local emit_changed = not quiet
		if not ok then
			local failure = pol
			if
				mare.proc.sandbox.generation() ~= before_generation
				or not same_state(before_policy, mare.proc.sandbox.current())
			then
				error("sandbox mode restore failed after changing policy: " .. tostring(failure), 0)
			end
			local fallback_generation = mare.proc.sandbox.generation()
			local fallback_policy = mare.proc.sandbox.current()
			local restored, fallback = pcall(critical_call, mare.proc.sandbox.set_current, sandbox_base or nil)
			if not restored then
				local current = mare.proc.sandbox.current()
				if
					mare.proc.sandbox.generation() ~= fallback_generation
					or not same_state(fallback_policy, current)
				then
					error("sandbox default policy restore failed after changing policy: " .. tostring(fallback), 0)
				end
				local host_generation = mare.proc.sandbox.generation()
				local host_policy = mare.proc.sandbox.current()
				local host_restored, host_fallback, omitted_default, host_snapshot =
					pcall(restore_sandbox_base, sandbox_base_snapshot or false)
				if not host_restored then
					local after = mare.proc.sandbox.current()
					if mare.proc.sandbox.generation() ~= host_generation or not same_state(host_policy, after) then
						error(
							"host sandbox default restore failed after changing policy: " .. tostring(host_fallback),
							0
						)
					end
					if
						not same_state(after, sandbox_base or nil)
						or tostring(host_fallback):find("SANDBOX_INVALID_ROOT", 1, true)
					then
						error(
							"sandbox default policy cannot be confirmed after both restore paths failed: "
								.. tostring(host_fallback),
							0
						)
					end
					pol = after
					emit_changed = false
					if not quiet then
						warn(
							"session sandbox mode not restored: "
								.. tostring(failure)
								.. "; Lua and host setters failed before mutation; retained the known default policy: "
								.. tostring(host_fallback)
						)
					end
				else
					pol = host_fallback
					sandbox_base = host_fallback or false
					sandbox_base_snapshot = host_snapshot or false
					if not quiet then
						warn(
							"session sandbox mode not restored: "
								.. tostring(failure)
								.. "; restored the default policy through the captured host PolicyUd snapshot after Lua fallback failed: "
								.. tostring(fallback)
						)
						for _, path in ipairs(omitted_default or {}) do
							warn("sandbox additional writable directory no longer exists: " .. path)
						end
					end
				end
			else
				pol = fallback
				if not quiet then
					warn("session sandbox mode not restored: " .. tostring(failure) .. "; using the default policy")
				end
			end
		end
		for _, path in ipairs(omitted) do
			if not mare.proc.sandbox.root_missing(path) then
				error("sandbox additional writable directory changed during restoration: " .. path, 0)
			end
			if not quiet then
				warn("sandbox additional writable directory no longer exists: " .. path)
			end
		end
		if emit_changed then
			ctx.bus:emit("sandbox/changed", { policy = pol })
		end
		return pol
	end

	local load_opened -- the rest of load_file, once the open was tried
	function D.prepare_settings(leaf, history_mod, snapshot)
		local target = snapshot or S
		local hist = history_mod or D.history
		local settings = hist.settings_for(leaf)
		if target.restore_cwd ~= false then
			local cwd = settings.cwd or target.resume_cwd or mare.cwd
			local st = mare.fs.stat(cwd)
			if not (st and st.kind == "dir") then
				settings.cwd_warning = "the selected shell directory no longer exists: " .. tostring(cwd)
				cwd = target.resume_cwd or mare.cwd
			end
			settings.shell_cwd = cwd
		end
		return settings
	end
	function D.precheck_sandbox_switch(leaf, history_mod, snapshot, restore_sandbox_ceiling, precomputed_settings)
		capture_sandbox_base()
		local preview = precomputed_settings or D.prepare_settings(leaf, history_mod, snapshot)
		local sb_ceiling = restore_sandbox_ceiling
		if sb_ceiling == nil and mare.args and type(mare.args.restore_sandbox_ceiling) == "table" then
			sb_ceiling = mare.args.restore_sandbox_ceiling
		end
		local target = preview and preview.sandbox or sandbox_base or nil
		if type(sb_ceiling) == "table" then
			if
				target == nil
				or target == false
				or (type(target) == "table" and target.level == "danger-full-access")
			then
				target = sb_ceiling
			else
				local ok_cap, capped = pcall(mare.proc.sandbox.intersect, target, sb_ceiling)
				target = (ok_cap and capped ~= nil) and capped or sb_ceiling
			end
		end
		if target ~= nil or sandbox_base ~= nil or type(sb_ceiling) == "table" then
			local ok_sb, err_sb = pcall(mare.proc.sandbox.check_set, target or nil)
			if not ok_sb and tostring(err_sb):find("PTY_POLICY_BUSY", 1, true) then
				require("mare_lib").fail("busy", tostring(err_sb))
			end
		end
	end

	function D.restore_settings(leaf, settings, restore_sandbox_ceiling)
		settings = settings or D.prepare_settings(leaf)
		local sb_ceiling = restore_sandbox_ceiling
		if sb_ceiling == nil and mare.args and type(mare.args.restore_sandbox_ceiling) == "table" then
			sb_ceiling = mare.args.restore_sandbox_ceiling
		end
		local function cap_sandbox_spec(spec)
			if type(sb_ceiling) ~= "table" then
				return spec
			end
			if spec == nil or spec == false or (type(spec) == "table" and spec.level == "danger-full-access") then
				return sb_ceiling
			end
			local ok_cap, capped = pcall(mare.proc.sandbox.intersect, spec, sb_ceiling)
			if ok_cap and capped ~= nil then
				return capped
			end
			return sb_ceiling
		end
		if settings.cwd_warning then
			warn(settings.cwd_warning)
		end
		S.shell_cwd = settings.shell_cwd
		local before_ws = mare.workspace.dirs()
		if
			type(settings.workspace_dirs) == "table"
			and #settings.workspace_dirs > 0
			and (settings.sandbox or sandbox_base ~= nil or type(sb_ceiling) == "table")
		then
			local pre_drop = {}
			for _, path in ipairs(settings.workspace_dirs) do
				if type(path) == "string" and path ~= "" then
					pre_drop[path] = true
				end
			end
			sandbox_apply(cap_sandbox_spec(settings.sandbox), pre_drop, true)
		end
		local kept_dirs, dropped_roots, seen_dirs, canon_to_raw = {}, {}, {}, {}
		if type(settings.workspace_dirs) == "table" then
			for _, path in ipairs(settings.workspace_dirs) do
				if type(path) == "string" and path ~= "" then
					local canon, verr = mare.workspace.check(path)
					if canon then
						if not seen_dirs[canon] then
							seen_dirs[canon] = true
							canon_to_raw[canon] = path
							kept_dirs[#kept_dirs + 1] = canon
						end
					else
						dropped_roots[path] = true
						warn(
							("additional workspace directory is no longer valid and was removed: %s (%s)"):format(
								path,
								tostring(verr)
							)
						)
					end
				end
			end
		end
		if type(sb_ceiling) == "table" and #kept_dirs > 0 then
			local allowed_dirs = {}
			local ceiling_roots = sb_ceiling.level == "read-only" and {} or (sb_ceiling.roots or {})
			for _, canon in ipairs(kept_dirs) do
				if sb_ceiling.level ~= "read-only" and mare.proc.sandbox.within(canon, ceiling_roots) then
					allowed_dirs[#allowed_dirs + 1] = canon
				else
					local raw = canon_to_raw[canon] or canon
					dropped_roots[canon] = true
					dropped_roots[raw] = true
					warn(
						("additional workspace directory exceeds the active sandbox ceiling and was removed: %s"):format(
							tostring(raw)
						)
					)
				end
			end
			kept_dirs = allowed_dirs
		end
		local ok_ws, after_ws = pcall(mare.workspace.set_dirs, kept_dirs)
		if not ok_ws then
			-- Cross-root protected-subpath overlap among saved roots: keep only mutually compatible roots.
			local filtered = {}
			for _, canon in ipairs(kept_dirs) do
				local peers = {}
				for _, other in ipairs(kept_dirs) do
					if other ~= canon then
						peers[#peers + 1] = other
					end
				end
				local ok_one = mare.workspace.check(canon, peers)
				if ok_one then
					filtered[#filtered + 1] = canon
				else
					local raw = canon_to_raw[canon] or canon
					dropped_roots[canon] = true
					dropped_roots[raw] = true
					warn(
						("additional workspace directory is inside a protected path and was removed: %s"):format(
							tostring(raw)
						)
					)
				end
			end
			kept_dirs = filtered
			ok_ws, after_ws = pcall(mare.workspace.set_dirs, kept_dirs)
			if not ok_ws then
				kept_dirs = {}
				after_ws = mare.workspace.set_dirs({})
			end
		end
		S.workspace_dirs = #kept_dirs > 0 and kept_dirs or nil
		mare.g["session.workspace_dirs"] = S.workspace_dirs and table.copy(S.workspace_dirs) or nil
		if after_ws and not same_state(before_ws, after_ws) then
			ctx.bus:emit("workspace/changed", { dirs = after_ws })
		end
		if settings.sandbox or sandbox_base ~= nil or type(sb_ceiling) == "table" then
			local target_sb
			if settings.sandbox ~= nil then
				target_sb = cap_sandbox_spec(settings.sandbox)
			elseif type(sb_ceiling) == "table" then
				target_sb = cap_sandbox_spec(sandbox_base)
			end
			local applied = sandbox_apply(target_sb, dropped_roots)
			S.sandbox_mode = settings.sandbox and (applied or { level = "danger-full-access" }) or nil
		else
			S.sandbox_mode = nil
		end
		if D.follow.llm() then
			local rec = settings.model
			local src = mare.config_sources and (mare.config_sources() or {}).model
			local requested = src and src.layer == "cli" and mare.config.model or nil
			local source_model = rec and rec.model or nil
			if not source_model and requested then
				-- An explicit model record on this path is authoritative. With none,
				-- the last assistant's current-format metadata identifies the source.
				local path = D.history.path_to(leaf)
				for i = #path, 1, -1 do
					local message = path[i].type == "message" and path[i].message
					if message and message.role == "assistant" and message.model_id and message.provider_id then
						source_model = message.provider_id .. "/" .. message.model_id
						break
					end
				end
			end
			local msgs = ctx.agent:messages()
			local source_unknown = false
			if requested and not source_model and #msgs > 0 then
				source_unknown = true
			end
			if not requested or rec or source_model then
				restoring_model = true
				local restoring <close> = setmetatable({}, {
					__close = function()
						restoring_model = false
					end,
				})
				local restored_default_model = false
				if not rec then
					if not requested then
						restored_default_model = restore_default_model()
					end
					if not restored_default_model then
						restore_default_effort("branch has no effort record")
					end
				end
				if source_model then
					local llm = D.follow.llm()
					local restored, err = recoverable_llm_call(llm.restore_selection, llm, { model = source_model })
					if not restored then
						warn("session model not restored: " .. tostring(err))
						if requested then
							source_unknown = true
						else
							restored_default_model = restore_default_model()
						end
					end
				end
				if rec and not rec.effort and not requested and not restored_default_model then
					restore_default_effort("branch has no effort record")
				end
				if rec and rec.effort then
					local llm = D.follow.llm()
					local restored, err = recoverable_llm_call(llm.restore_selection, llm, { effort = rec.effort })
					if not restored then
						restore_default_effort("session effort not restored: " .. tostring(err))
					end
				end
			end
			if requested then
				-- CLI/child overrides must pass the same budget gate as Lua and RPC.
				-- Failure leaves the restored model in place and blocks startup requests.
				restoring_model = false
				local llm = D.follow.llm()
				critical_call(llm.set_model, llm, requested, {
					target = "session",
					source_unknown = source_unknown or nil,
				})
			end
		end
	end

	local function load_file(path, known, opts)
		local skip_policy = type(opts) == "table" and opts.skip_policy == true
		if not skip_policy then
			local before =
				ctx.policy:decide("session/before-switch", table.pack({ reason = "resume", target_path = path }))
			if not before or before.decision ~= "allow" then
				require("mare_lib").fail("cancelled", (before and before.reason) or "session switch cancelled")
			end
		end
		local lease <close> = D.store.begin_switch()
		lease.end_reason = "switch"
		D.store.require_idle()
		local owner = S.id
		local drained, drain_err = D.store.drain_side(nil, owner)
		if not drained then
			if mare.ui.connected() then
				ui.notify(drain_err, "warning")
			end
			require("mare_lib").fail("busy", drain_err)
		end
		-- one writer per session: take the lock first — `open` validates the file,
		-- checks its format under the lock — and only then read. A file
		-- locked elsewhere or damaged (a bad line that isn't a torn tail) is left untouched; the
		-- user chooses a read-only view or a fork.
		-- --no-session (and a child agent) persist nothing: `open` would take the
		-- lock the file and the session would then append to it — refused
		-- before anything touches the file
		if disabled then
			error(
				mare.mode == "child" and "a child agent doesn't resume sessions"
					or "--no-session: sessions are neither resumed nor written",
				0
			)
		end
		D.store.require_idle()
		-- Keep the current writer open while reading its own session. A concurrent turn must not
		-- lose its journal or fail because this load temporarily owns its lock.
		local revision = D.store.revision()
		local reuse = S.file ~= nil and S.file:matches_path(path)
		local opened, f
		if reuse then
			lease.reuse_path = path
			opened, f = true, S.file
		else
			-- `existing`: a missing path never creates a new headerless session.
			opened, f = pcall(mare.store.open, path, { existing = true, defer_activation = true })
			if opened then
				lease.target = f
				lease.discard_target = true
			end
		end
		-- Anything failing before handing the opened handle to S.file releases it now.
		local caller = type(opts) == "table" and opts.caller or nil
		local from_reload = type(opts) == "table" and opts._from_reload == true
		local restore_ceiling = type(opts) == "table"
				and (opts.restore_permission_ceiling ~= nil or opts.restore_sandbox_ceiling ~= nil)
				and {
					permission = opts.restore_permission_ceiling,
					sandbox = opts.restore_sandbox_ceiling,
				}
			or nil
		local res = table.pack(
			pcall(load_opened, path, known, opened, f, revision, reuse, lease, caller, from_reload, restore_ceiling)
		)
		if not res[1] then
			error(res[2], 0)
		end
		return table.unpack(res, 2, res.n)
	end
	load_opened = function(path, known, opened, f, revision, reuse, lease, caller, from_reload, restore_ceiling)
		local load_helpers = require("session.load_helpers")
		local fork_it, readonly, lenient = false, false, false
		if not opened then
			fork_it, readonly, lenient = load_helpers.resolve_unopened_mode(D, path, known, f)
		end
		local tail, skipped, stable = load_helpers.read_session_tail(D, path, known, opened, f, lease, fork_it, lenient)
		local active = S
		local snap = D.store.new_snapshot()
		local history = require("session.history")({ ctx = ctx, S = snap, disabled = disabled, store = D.store })
		snap.path = path
		snap.lenient = lenient or nil
		snap.partial = tail.partial
		if tail.header and tail.header.session_id then
			snap.id = tail.header.session_id
			snap.parent_session_id = tail.header.parent_session_id
			snap.created_at_ms = tail.header.created_at_ms
		end
		local readonly_opened, preparation_warnings
		readonly, readonly_opened, preparation_warnings =
			load_helpers.resolve_resume_cwd(cfg, tail, path, opened, f, reuse, lease, snap, readonly)
		local source_path = path
		local msgs, made_fork, precomputed_settings, usage_stale
		snap, msgs, f, path, opened, readonly, made_fork, precomputed_settings, usage_stale =
			load_helpers.hydrate_snapshot(
				D,
				ctx,
				disabled,
				snap,
				history,
				tail,
				path,
				known,
				opened,
				f,
				lease,
				fork_it,
				readonly,
				readonly_opened,
				stable
			)
		-- Verify nothing mutated the outgoing session during target preparation before
		-- cancelling background subagents, then drain and capture the post-drain revision.
		D.store.verify_revision(lease, revision)
		D.store.drain_subagents()
		if S.file and S.file:matches_path(S.path) then
			S.file:flush()
		end
		if
			reuse
			and opened
			and f
			and lease.target_revision
			and not mare.store.snapshot_matches(f, lease.target_revision)
		then
			local saved_resume_cwd, saved_restore_cwd, saved_shell_cwd =
				snap.resume_cwd, snap.restore_cwd, snap.shell_cwd
			tail, skipped, stable = load_helpers.read_session_tail(D, path, known, opened, f, lease, fork_it, lenient)
			if lease.source_file and rawequal(lease.source_file, f) then
				lease.source_revision = tail.revision
			end
			snap = D.store.new_snapshot()
			history = require("session.history")({ ctx = ctx, S = snap, disabled = disabled, store = D.store })
			snap.path = path
			snap.lenient = lenient or nil
			snap.partial = tail.partial
			if tail.header and tail.header.session_id then
				snap.id = tail.header.session_id
				snap.parent_session_id = tail.header.parent_session_id
				snap.created_at_ms = tail.header.created_at_ms
			end
			snap.resume_cwd, snap.restore_cwd, snap.shell_cwd = saved_resume_cwd, saved_restore_cwd, saved_shell_cwd
			snap, msgs, f, path, opened, readonly, made_fork, precomputed_settings, usage_stale =
				load_helpers.hydrate_snapshot(
					D,
					ctx,
					disabled,
					snap,
					history,
					tail,
					path,
					known,
					opened,
					f,
					lease,
					fork_it,
					readonly,
					readonly_opened,
					stable
				)
		end
		revision = D.store.revision()
		-- Pre-check persistent PTY fence BEFORE `publish` so `PTY_POLICY_BUSY` fails cleanly
		-- without entering post-publish `critical = true` and poisoning the VM.
		D.precheck_sandbox_switch(
			snap.leaf,
			history,
			snap,
			restore_ceiling and restore_ceiling.sandbox or nil,
			precomputed_settings
		)
		if D.ctx and D.ctx.kernel and D.ctx.kernel.begin_session_switch then
			lease.identity_switch = D.ctx.kernel:begin_session_switch({ session_id = snap.id })
		end
		D.store.publish(lease, snap, revision, msgs)
		return load_helpers.run_finish_switch(D, ctx, active, critical_call, changed, lease, {
			preparation_warnings = preparation_warnings,
			opened = opened,
			made_fork = made_fork,
			f = f,
			readonly = readonly,
			msgs = msgs,
			path = path,
			skipped = skipped,
			fork_it = fork_it,
			source_path = source_path,
			precomputed_settings = precomputed_settings,
			usage_stale = usage_stale,
			restore_ceiling = restore_ceiling,
		})
	end
	function session.switching(_, _)
		return D.store.switching()
	end
	local function normalize_selector_title(s)
		if type(s) ~= "string" then
			return ""
		end
		local collapsed = s:gsub("%s+", " "):gsub("^%s+", ""):gsub("%s+$", ""):lower()
		return collapsed:sub(1, 160)
	end

	local function resolve_load_target(self, caller, opts)
		local lib = require("mare_lib")
		local sel_id, sel_path = opts.session_id, opts.path
		if (sel_id == nil and sel_path == nil) or (sel_id ~= nil and sel_path ~= nil) then
			lib.fail("invalid_arguments", "session.load requires exactly one of `session_id` or `path`")
		end
		local cross_project = not mare.contracts.is_peer(caller)
		if sel_path ~= nil then
			if type(sel_path) ~= "string" or sel_path == "" then
				lib.fail("invalid_arguments", "path must be a non-empty string")
			end
			if not mare.contracts.allows_session_path(caller) then
				lib.fail("denied", "restricted callers cannot address sessions by file path")
			end
			local resolved = mare.fs.resolve(sel_path)
			if not mare.fs.exists(resolved) then
				lib.fail("unknown_session", ("session file not found: %s"):format(sel_path))
			end
			return resolved
		end
		if type(sel_id) ~= "string" or sel_id == "" then
			lib.fail("invalid_arguments", "session_id must be a non-empty string")
		end
		---@cast sel_id string
		local path_like = sel_id:find("[/\\]") or sel_id:find("%.jsonl$")
		if path_like then
			if not mare.contracts.allows_session_path(caller) then
				lib.fail("denied", "restricted callers cannot address sessions by file path")
			end
			local resolved = mare.fs.resolve(sel_id)
			if mare.fs.exists(resolved) then
				return resolved
			end
		end
		local can_list = mare.contracts.permitted("session", "list", caller)
		local list_opts_active = cross_project and { all = true } or {}
		local list_opts_archived = cross_project and { all = true, archived = true } or { archived = true }
		local active_res = self:list(caller, list_opts_active)
		local archived_res = self:list(caller, list_opts_archived)
		local rows, seen_rows = {}, {}
		for _, batch in ipairs({ active_res and active_res.data or {}, archived_res and archived_res.data or {} }) do
			for _, r in ipairs(batch) do
				if type(r) == "table" and type(r.path) == "string" and r.path ~= "" and not seen_rows[r.path] then
					seen_rows[r.path] = true
					rows[#rows + 1] = r
				end
			end
		end
		local norm_query = normalize_selector_title(sel_id)
		local hits, seen = {}, {}
		for _, r in ipairs(rows) do
			local id = r.session_id
			if id == sel_id then
				if r.archived then
					lib.fail("invalid_arguments", ("session `%s` is archived — unarchive it first"):format(sel_id))
				end
				return r.path
			end
			local by_prefix = type(id) == "string" and id:sub(1, #sel_id) == sel_id
			local title = normalize_selector_title(r.name)
			local by_name = norm_query ~= "" and title == norm_query
			if (by_prefix or by_name) and not seen[r.path] then
				seen[r.path] = true
				hits[#hits + 1] = r
			end
		end
		if #hits == 0 then
			lib.fail("invalid_arguments", ("no session matches `%s`"):format(sel_id))
		elseif #hits == 1 then
			if hits[1].archived then
				local short = type(hits[1].session_id) == "string" and hits[1].session_id:sub(1, 8) or sel_id
				if not can_list then
					lib.fail("invalid_arguments", "that session is archived — unarchive it first")
				end
				lib.fail("invalid_arguments", ("session `%s` is archived — unarchive it first"):format(short))
			end
			return hits[1].path
		end
		if not can_list then
			lib.fail("invalid_arguments", ("`%s` matches more than one session — give more of the id"):format(sel_id))
		end
		local parts = {}
		for i = 1, math.min(5, #hits) do
			local h = hits[i]
			local short = type(h.session_id) == "string" and h.session_id:sub(1, 8) or ""
			if type(h.name) == "string" and h.name ~= "" then
				parts[#parts + 1] = ("%s (%s)"):format(h.name, short)
			else
				parts[#parts + 1] = short
			end
		end
		local suffix = #hits > 5 and ", …" or ""
		lib.fail(
			"invalid_arguments",
			("`%s` matches %d sessions: %s%s — give more of the id"):format(
				sel_id,
				#hits,
				table.concat(parts, ", "),
				suffix
			)
		)
	end

	local function new_session(opts)
		local before = ctx.policy:decide("session/before-switch", table.pack({ reason = "new" }))
		if not before or before.decision ~= "allow" then
			require("mare_lib").fail("cancelled", (before and before.reason) or "session switch cancelled")
		end
		local lease <close> = D.store.begin_switch()
		lease.end_reason = "new"
		D.store.require_idle()
		local expected = type(opts) == "table" and opts.expected_session or nil
		if expected then
			if
				S.id ~= expected.id
				or S.generation ~= expected.generation
				or S.path ~= expected.path
				or S.file ~= expected.file
			then
				require("mare_lib").fail("stale_session", "active session changed before switch")
			end
		end
		local owner = S.id
		local drained, drain_err = D.store.drain_side(nil, owner)
		if not drained then
			if mare.ui.connected() then
				ui.notify(drain_err, "warning")
			end
			require("mare_lib").fail("busy", drain_err)
		end
		D.store.drain_subagents()
		if type(opts) == "table" and opts.close_entries then
			lease.close_entries = opts.close_entries
		end
		local revision = D.store.revision()
		local snapshot =
			D.store.new_snapshot(type(opts) == "table" and opts.cwd or nil, type(opts) == "table" and opts.id or nil)
		-- Pre-check persistent PTY fence BEFORE `publish` so `PTY_POLICY_BUSY` fails cleanly
		-- without entering post-publish `critical = true` and poisoning the VM.
		capture_sandbox_base()
		if sandbox_base ~= nil then
			local ok_sb, err_sb = pcall(mare.proc.sandbox.check_set, sandbox_base or nil)
			if not ok_sb and tostring(err_sb):find("PTY_POLICY_BUSY", 1, true) then
				require("mare_lib").fail("busy", tostring(err_sb))
			end
		end
		if D.ctx and D.ctx.kernel and D.ctx.kernel.begin_session_switch then
			lease.identity_switch = D.ctx.kernel:begin_session_switch({ session_id = snapshot.id })
		end
		D.store.publish(lease, snapshot, revision, {})
		return D.store.finish_switch(lease, function(attempt)
			attempt(function()
				S.workspace_dirs = nil
				mare.g["session.workspace_dirs"] = nil
				local before_ws = mare.workspace.dirs()
				local after_ws = mare.workspace.set_dirs({})
				if not same_state(before_ws, after_ws) then
					ctx.bus:emit("workspace/changed", { dirs = after_ws })
				end
				if sandbox_base ~= nil then
					sandbox_apply(nil)
				end
			end, true)
			attempt(function()
				ctx.bus:emit("agent/messages-replaced", { messages = {} })
			end)
			attempt(function()
				changed("new")
			end)
			attempt(function()
				ctx.bus:emit("session/new", {
					session_id = S.id,
					path = S.planned,
					restore_permission_ceiling = opts and opts.restore_permission_ceiling or nil,
					restore_sandbox_ceiling = opts and opts.restore_sandbox_ceiling or nil,
				})
			end)
		end)
	end
	local function switch_session(self, caller, opts)
		local lib = require("mare_lib")
		if type(opts) ~= "table" or type(opts.kind) ~= "string" then
			lib.fail("invalid_arguments", "session.switch requires { kind = 'new' | 'load' | 'fork', ... }")
		end
		if opts.kind == "new" then
			local target_cwd
			if opts.cwd ~= nil then
				if type(opts.cwd) ~= "string" or opts.cwd == "" then
					lib.fail("invalid_arguments", "cwd must be a non-empty string")
				end
				local resolved = D.normalize_cwd(opts.cwd)
				local host_cwd = D.normalize_cwd(mare.cwd)
				if resolved ~= host_cwd then
					local st = mare.fs.stat(resolved)
					if not st or st.kind ~= "dir" then
						lib.fail("invalid_arguments", "cwd does not exist or is not a directory")
					end
					target_cwd = resolved
				end
			end
			if opts.name ~= nil and type(opts.name) ~= "string" then
				lib.fail("invalid_arguments", "name must be a string")
			end
			if opts.new_session_id ~= nil and (type(opts.new_session_id) ~= "string" or opts.new_session_id == "") then
				lib.fail("invalid_arguments", "new_session_id must be a non-empty string")
			end
			new_session({
				cwd = target_cwd,
				id = opts.new_session_id,
				restore_permission_ceiling = opts.restore_permission_ceiling,
				restore_sandbox_ceiling = opts.restore_sandbox_ceiling,
			})
			if opts.name ~= nil and opts.name ~= "" then
				local ok, err = D.set_name(opts.name)
				if not ok then
					lib.fail("persist_failed", tostring(err))
				end
			end
			return D.current_session_snapshot()
		elseif opts.kind == "load" then
			local path
			if opts.path ~= nil then
				if type(opts.path) ~= "string" or opts.path == "" then
					lib.fail("invalid_arguments", "path must be a non-empty string")
				end
				local resolved = mare.fs.resolve(opts.path)
				if not mare.fs.exists(resolved) then
					lib.fail("unknown_session", ("session file not found: %s"):format(opts.path))
				end
				path = resolved
			elseif opts.session_id ~= nil then
				path = resolve_load_target(self, caller, { session_id = opts.session_id })
			else
				lib.fail("invalid_arguments", "session.switch load requires `path` or `session_id`")
			end
			load_file(path, nil, {
				caller = caller,
				restore_permission_ceiling = opts.restore_permission_ceiling,
				restore_sandbox_ceiling = opts.restore_sandbox_ceiling,
			})
			return D.current_session_snapshot()
		elseif opts.kind == "fork" then
			return D.fork.switch_fork(caller, opts)
		end
		lib.fail("invalid_arguments", "session.switch kind must be 'new', 'load', or 'fork'")
	end

	function session.switch(self, caller, opts)
		return switch_session(self, caller, opts)
	end
	--- set_sandbox_mode(spec) -> policy | nil (unconfined): switch this session's sandbox policy
	--- (`mare.proc.sandbox.set_current` spec). Logged in the session, effective from the next
	--- call, restored on resume. Errors on an invalid spec (e.g. a bad extra root) or persist failure.
	function session.set_sandbox_mode(_, _, spec)
		capture_sandbox_base()
		if mare.exec.current() then
			error("session write failed: session publication is unavailable inside a tool call", 0)
		end
		local err_reason = mare.task.session_error()
		if err_reason then
			error("session write failed: " .. tostring(err_reason), 0)
		end
		local validated = mare.proc.sandbox.snapshot_policy(spec)
		local mode_record = validated or { level = "danger-full-access" }
		local _, prev_snap = mare.proc.sandbox.snapshot_current()
		local prev_mode = S.sandbox_mode
		mare.proc.sandbox.check_set(spec)
		local applied_pol
		local ok, err = add(
			{
				type = "custom",
				kind = "sandbox/mode",
				data = mode_record,
				provenance = "session-host",
				at_entry_id = S.leaf or false,
				created_at_ms = require("mare_lib").to_epoch_ms(mare.task.now()),
			},
			S.file ~= nil,
			S.file ~= nil,
			nil,
			function()
				local ok_set, pol_or_err = pcall(mare.proc.sandbox.set_current, spec)
				if not ok_set then
					pcall(restore_sandbox_base, prev_snap or false)
					S.sandbox_mode = prev_mode
					error(pol_or_err, 0)
				end
				applied_pol = pol_or_err
				S.sandbox_mode = applied_pol or { level = "danger-full-access" }
			end
		)
		if not ok then
			error("session write failed: " .. tostring(err), 0)
		end
		ctx.bus:emit("sandbox/changed", { policy = applied_pol })
		return applied_pol
	end

	local function build_extended_sandbox_spec(cur_pol, added_dirs)
		local spec = {
			level = cur_pol.level,
			cwd = cur_pol.cwd,
			exclude_tmpdir_env_var = cur_pol.exclude_tmpdir_env_var,
			exclude_slash_tmp = cur_pol.exclude_slash_tmp,
			readable = cur_pol.readable and table.copy(cur_pol.readable) or {},
			deny_read = cur_pol.deny_read and table.copy(cur_pol.deny_read) or {},
			restrict_read = cur_pol.restrict_read,
			reads_pinned = cur_pol.reads_pinned,
			profile = cur_pol.profile,
			network = cur_pol.network,
			unix_sockets = cur_pol.unix_sockets and table.copy(cur_pol.unix_sockets) or nil,
			accept_partial = cur_pol.accept_partial and table.copy(cur_pol.accept_partial) or nil,
		}
		if cur_pol.write ~= nil then
			local roots = table.copy(cur_pol.write)
			for _, dir_path in ipairs(added_dirs) do
				roots[#roots + 1] = dir_path
			end
			spec.write = roots
		else
			local roots = cur_pol.writable and table.copy(cur_pol.writable) or {}
			for _, dir_path in ipairs(added_dirs) do
				roots[#roots + 1] = dir_path
			end
			spec.writable = roots
		end
		return spec
	end

	local add_dirs_busy = false
	local add_dirs_waiters = {}
	local function run_add_dirs_serialized(fn)
		if add_dirs_busy then
			if not coroutine.isyieldable() then
				require("mare_lib").fail("busy", "concurrent workspace directory update in progress")
			end
			local tx, rx = mare.task.channel()
			add_dirs_waiters[#add_dirs_waiters + 1] = tx
			rx:recv()
		end
		add_dirs_busy = true
		local _guard <close> = setmetatable({}, {
			__close = function()
				while #add_dirs_waiters > 0 do
					local next_tx = table.remove(add_dirs_waiters, 1)
					if next_tx and next_tx:send(true) then
						return
					end
				end
				add_dirs_busy = false
			end,
		})
		return fn()
	end

	--- add_workspace_dirs(dirs) -> { dirs, sandbox? }: atomically validate, persist, and publish
	--- additional workspace directories (and extend `workspace-write` sandbox roots when active).
	function session.add_workspace_dirs(_, _, dirs)
		return run_add_dirs_serialized(function()
			local lib = require("mare_lib")
			if type(dirs) ~= "table" then
				lib.fail("invalid_arguments", "add_workspace_dirs requires an array of directory paths")
			end
			capture_sandbox_base()
			local current_dirs = mare.workspace.dirs()
			local cwd_canon = current_dirs[1] or mare.cwd
			local next_extra = S.workspace_dirs and table.copy(S.workspace_dirs) or {}
			local seen = { [cwd_canon] = true }
			for _, existing in ipairs(next_extra) do
				seen[existing] = true
			end
			local added = {}
			for _, raw in ipairs(dirs) do
				if type(raw) ~= "string" or raw == "" then
					lib.fail("invalid_arguments", "workspace directory path must be a non-empty string")
				end
				local canon, verr = mare.workspace.check(raw)
				if not canon then
					lib.fail("invalid_arguments", tostring(verr))
				end
				if not seen[canon] then
					seen[canon] = true
					next_extra[#next_extra + 1] = canon
					added[#added + 1] = canon
				end
			end
			if #added == 0 then
				return { dirs = mare.workspace.dirs(), sandbox = mare.proc.sandbox.current() }
			end
			-- Enforce pairwise protected-subpath isolation across `cwd` + `next_extra` via Rust.
			for _, extra_dir in ipairs(next_extra) do
				local ok_dir, verr = mare.workspace.check(extra_dir, next_extra)
				if not ok_dir then
					lib.fail("invalid_arguments", tostring(verr))
				end
			end
			local cur_pol = mare.proc.sandbox.current()
			local candidate_spec, candidate_pol
			if cur_pol and cur_pol.level == "workspace-write" then
				candidate_spec = build_extended_sandbox_spec(cur_pol, added)
				local snap_pol = mare.proc.sandbox.snapshot_policy(candidate_spec)
				if type(snap_pol) ~= "table" or snap_pol.level ~= "workspace-write" then
					lib.fail("denied", "sandbox ceiling rejected additional workspace directory")
				end
				for _, added_dir in ipairs(added) do
					if not mare.proc.sandbox.within(added_dir, snap_pol.roots or {}) then
						lib.fail(
							"denied",
							"additional workspace directory is outside the active sandbox ceiling: " .. added_dir
						)
					end
				end
				candidate_pol = snap_pol
			end
			local now = lib.to_epoch_ms(mare.task.now())
			local batch = {
				{
					type = "custom",
					kind = "workspace/dirs",
					data = { dirs = next_extra },
					provenance = "session-host",
					at_entry_id = S.leaf or false,
					created_at_ms = now,
				},
			}
			if candidate_pol ~= nil then
				batch[#batch + 1] = {
					type = "custom",
					kind = "sandbox/mode",
					data = candidate_pol,
					provenance = "session-host",
					at_entry_id = S.leaf or false,
					created_at_ms = now,
				}
			end
			local prev_extra = S.workspace_dirs and table.copy(S.workspace_dirs) or {}
			local _, prev_snap = mare.proc.sandbox.snapshot_current()
			local prev_mode = S.sandbox_mode
			local applied_dirs, applied_pol
			local ok, err = D.store.add_batch(batch, nil, function()
				applied_dirs = mare.workspace.set_dirs(next_extra)
				if candidate_spec ~= nil then
					local ok_sb, pol_or_err = pcall(mare.proc.sandbox.set_current, candidate_spec)
					if not ok_sb then
						pcall(mare.workspace.set_dirs, prev_extra)
						pcall(restore_sandbox_base, prev_snap or false)
						S.workspace_dirs = #prev_extra > 0 and prev_extra or nil
						mare.g["session.workspace_dirs"] = S.workspace_dirs and table.copy(S.workspace_dirs) or nil
						S.sandbox_mode = prev_mode
						error(pol_or_err, 0)
					end
					applied_pol = pol_or_err
					S.sandbox_mode = applied_pol or { level = "danger-full-access" }
				end
				S.workspace_dirs = next_extra
				mare.g["session.workspace_dirs"] = table.copy(next_extra)
			end, {
				persist = S.file ~= nil,
				sync = S.file ~= nil,
				allow_readonly = true,
			})
			if not ok then
				lib.fail("persist_failed", "session write failed: " .. tostring(err))
			end
			ctx.bus:emit("workspace/changed", { dirs = applied_dirs })
			if candidate_spec ~= nil then
				ctx.bus:emit("sandbox/changed", { policy = applied_pol })
			end
			return { dirs = applied_dirs, sandbox = applied_pol or mare.proc.sandbox.current() }
		end)
	end
	-- Optional internal hook for the permissions plugin: set the host's configured baseline before
	-- applying its initial preset, even when the session service was loaded first with no policy.
	function session.set_default_sandbox_mode(_, _, spec)
		local policy, snapshot, omitted = mare.proc.sandbox.snapshot_policy(spec)
		sandbox_base = policy or false
		sandbox_base_snapshot = snapshot or false
		for _, path in ipairs(omitted) do
			warn("sandbox additional writable directory no longer exists: " .. path)
		end
		return true
	end
	ctx:on("llm/model-changed", function(ev)
		if ev.model and not restoring_model and not ev.scoped_only then
			local effort = ev.global_effort
			if effort == nil and ev.scope == nil then
				effort = ev.effort
			end
			local ok, err = add(
				{ type = "model", model = ev.model.ref, effort = effort, at_entry_id = S.leaf or false },
				S.file ~= nil
			)
			if not ok then
				-- not fatal (the next resume picks the default model), but never silent
				mare.log("warn", "model switch not recorded in the session: " .. tostring(err))
			end
		end
	end)

	return { load_file = load_file, new_session = new_session, switch_session = switch_session, reset = reset }
end
