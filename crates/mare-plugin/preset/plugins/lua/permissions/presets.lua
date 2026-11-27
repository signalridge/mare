-- permissions.presets: the preset tables (built-ins < `presets` option), presets → sandbox specs
-- and switching presets; part of the `permissions` plugin (plugins/permissions.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local P, lib, child = D.P, D.lib, D.child
	local record, existing_dirs = D.record, D.existing_dirs
	local sandbox = mare.proc.sandbox
	local sandbox_current = sandbox.current
	local snapshot_current = sandbox.snapshot_current
	local snapshot_policy = sandbox.snapshot_policy
	local restore_snapshot = sandbox.restore_snapshot
	local host_set_current = sandbox.set_current

	local RANK = { ["read-only"] = 1, auto = 2, ["full-access"] = 3 }
	local LEVEL = { ["read-only"] = "read-only", auto = "workspace-write", ["full-access"] = "danger-full-access" }
	local PRESETS = { "read-only", "auto", "full-access" }
	local LABEL = { ["read-only"] = "READ", auto = "AUTO", ["full-access"] = "FULL", custom = "CUSTOM" }
	local DESC = {
		["read-only"] = "read anywhere; writing files or going online needs your approval",
		auto = "write in this workspace and temp dirs; asks only when leaving the sandbox",
		["full-access"] = "no sandbox, never asks",
	}

	local function copy_table(t)
		local out = {}
		for k, v in pairs(t) do
			out[k] = v
		end
		return out
	end

	local function same_value(a, b)
		if type(a) ~= type(b) then
			return false
		end
		if type(a) ~= "table" then
			return a == b
		end
		for k, v in pairs(a) do
			if not same_value(v, b[k]) then
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
	local function same_policy(a, b)
		if a == false then
			a = nil
		end
		if b == false then
			b = nil
		end
		return same_value(a, b)
	end

	-- preset tables: built-ins < `presets` option, resolved through the single preset resolver
	local LEVEL_RANK = { ["read-only"] = 1, ["workspace-write"] = 2, ["danger-full-access"] = 3 }
	local APPROVAL_RANK = { ["read-only"] = 1, auto = 2, ["full-access"] = 3 }
	local RANK, LEVEL, LABEL, DESC, PRESETS, LIKE, EXTRA =
		copy_table(RANK), copy_table(LEVEL), copy_table(LABEL), copy_table(DESC), copy_table(PRESETS), {}, {}
	local BUILTIN_PRESETS = {}
	for _, name in ipairs(PRESETS) do
		LIKE[name] = name
		BUILTIN_PRESETS[name] = {
			level = LEVEL[name],
			label = LABEL[name],
			desc = DESC[name],
		}
	end
	local configured = cfg.presets or {}
	local _, _, meta = mare.config_resolve_preset({
		presets = BUILTIN_PRESETS,
		extra = configured,
		inherit_key = "like",
		stop_at = APPROVAL_RANK,
	}, nil)
	local names = {}
	for name in pairs(configured) do
		names[#names + 1] = name
	end
	table.sort(names)
	for _, name in ipairs(names) do
		local def = configured[name]
		if type(def) == "table" then
			local resolved, err, root_base = meta.resolve_one(name)
			local like
			if err then
				mare.log("warn", "permissions: " .. tostring(err) .. " for " .. tostring(name))
				like = "read-only"
			elseif root_base and APPROVAL_RANK[root_base] then
				like = root_base
			elseif APPROVAL_RANK[name] then
				like = name
			else
				like = "auto"
			end
			if not RANK[name] then
				PRESETS[#PRESETS + 1] = name
			end
			LIKE[name] = like
			LEVEL[name] = (resolved and resolved.level) or def.level or LEVEL[name] or LEVEL[like]
			RANK[name] = LEVEL_RANK[LEVEL[name]] or RANK[like]
			LABEL[name] = def.label or LABEL[name] or string.upper(name)
			DESC[name] = def.desc or DESC[name] or ("like " .. like)
			EXTRA[name] = resolved or def
		end
	end

	---------------------------------------------------------------------------
	-- presets → sandbox specs
	---------------------------------------------------------------------------
	-- Capabilities come from probing (child processes, started in the background at launch);
	-- they're looked up lazily so loading this plugin never waits for the probe. Without an
	-- explicit accept_partial option, missing fs_protect is the one default partial capability;
	-- the Windows default tier also implicitly accepts its existing network limitation.
	local CAPS
	local function caps()
		CAPS = CAPS or mare.proc.sandbox.capabilities()
		return CAPS
	end
	local function has_cap(name)
		for _, c in ipairs(caps().caps or {}) do
			if c == name then
				return true
			end
		end
		return false
	end
	local function no_net_control()
		return caps().mechanism ~= "none" and not has_cap("net_block") and not has_cap("net_proxy")
	end
	local function win_default()
		if cfg.accept_partial ~= nil then
			return false
		end
		-- Only the default Windows tier has implicit acceptance. An enhanced tier whose
		-- per-call network isolation is unavailable must require explicit acceptance.
		if mare.os == "windows" then
			return caps().mechanism == "windows-acl" and no_net_control()
		end
		-- Tests can simulate the default tier's capability set on other platforms.
		return (mare.env("MARE_SANDBOX_CAPS") or "") ~= "" and no_net_control()
	end
	local function accepted()
		if cfg.accept_partial ~= nil then
			return cfg.accept_partial
		end
		local defaults = win_default() and { "network" } or {}
		defaults[#defaults + 1] = "fs_protect"
		return defaults
	end
	local function accepts_network()
		for _, n in ipairs(accepted()) do
			if n == "network" or n == "net_block" or n == "net_proxy" then
				return true
			end
		end
		return false
	end

	--- options every sandbox spec carries (capability acceptance, listed sockets)
	local function with_common(spec)
		if type(spec) == "table" then
			spec.accept_partial = spec.accept_partial or accepted()
			spec.unix_sockets = spec.unix_sockets or cfg.unix_sockets
			spec.deny_read = spec.deny_read or cfg.deny_read
		end
		return spec
	end

	--- the sandbox spec of a preset (false = no sandbox)
	local function spec_for(preset)
		local def = EXTRA[preset] or {}
		local level = LEVEL[preset] or "workspace-write"
		if level == "danger-full-access" then
			return false
		elseif level == "read-only" then
			return with_common({
				level = "read-only",
				readable = def.readable or cfg.readable,
				deny_read = def.deny_read,
			})
		end
		local network = def.network
		if network == nil then
			network = cfg.network
		end
		local writable = existing_dirs(def.writable or cfg.writable)
		-- Only preserve session-validated writable workspace roots (never read-only mare.workspace.add entries).
		local session_dirs = mare.g and mare.g["session.workspace_dirs"]
		if type(session_dirs) == "table" and #session_dirs > 0 then
			local seen = {}
			for _, p in ipairs(writable) do
				seen[p] = true
			end
			for _, d in ipairs(session_dirs) do
				if type(d) == "string" and d ~= "" and not seen[d] then
					seen[d] = true
					writable[#writable + 1] = d
				end
			end
			writable = existing_dirs(writable)
		end
		return with_common({
			level = "workspace-write",
			writable = writable,
			readable = def.readable or cfg.readable,
			deny_read = def.deny_read,
			network = network,
		})
	end
	local function base_spec()
		return sandbox_current() or false
	end

	local function apply_sandbox(spec)
		local captured, previous, previous_snapshot = pcall(snapshot_current)
		if not captured then
			return false, "could not capture the current host sandbox policy: " .. tostring(previous)
		end
		local normalized, expected = pcall(function()
			return snapshot_policy(spec)
		end)
		if not normalized then
			return false, tostring(expected)
		end
		P.applying = true
		local result = table.pack(pcall(function()
			local session = D.svc.session
			if session and session.set_sandbox_mode then
				return session:set_sandbox_mode(spec)
			else
				return host_set_current(spec)
			end
		end))
		P.applying = false
		local failure
		if not result[1] then
			failure = tostring(result[2])
		elseif result[2] == false or (result[2] == nil and result.n > 2 and result[3] ~= nil) then
			failure = tostring(result[3] or "session sandbox setter refused the policy")
		elseif not same_policy(sandbox_current(), expected) then
			failure = "session sandbox setter returned without applying the requested policy"
		else
			if mare.proc and mare.proc.sandbox and mare.proc.sandbox.generation then
				P.sandbox_gen = mare.proc.sandbox.generation()
			end
			return true
		end

		-- A replacement session provider can expose the required setter but fail to apply its
		-- contract. Restore the last verified host policy before refusing future requests.
		local restored, restored_policy = pcall(restore_snapshot, previous_snapshot or false)
		if not restored then
			return false, failure .. "; previous host sandbox restore failed: " .. tostring(restored_policy)
		end
		if not same_policy(sandbox_current(), restored_policy) then
			return false, failure .. "; previous host sandbox policy could not be confirmed"
		end
		if mare.proc and mare.proc.sandbox and mare.proc.sandbox.generation then
			P.sandbox_gen = mare.proc.sandbox.generation()
		end
		return false, failure .. "; previous host sandbox policy restored"
	end

	--- Network-specific gap: used for the read-only shell approval warning.
	local function is_network_partial()
		if not (accepts_network() and no_net_control()) then
			return false
		end
		local cur = mare.proc.sandbox.current()
		return cur ~= nil and cur.network ~= true
	end
	-- Only accepted missing capabilities are reported as partial; the default fs_protect
	-- acceptance is added to the sandbox spec when accept_partial is not explicitly configured.
	local function is_partial()
		local cur = mare.proc.sandbox.current()
		if not cur then
			return false
		end
		local accepted = {}
		for _, cap in ipairs(cur.accept_partial or {}) do
			accepted[cap] = true
		end
		for _, cap in ipairs(cur.unavailable or {}) do
			if accepted[cap] then
				return true
			end
		end
		return false
	end
	local function emit()
		local partial = is_partial()
		ctx.bus:emit("permission/changed", {
			preset = P.preset,
			label = partial and "PARTIAL" or LABEL[P.preset],
			partial = partial or nil,
		})
	end

	local function approval_rank_of(name)
		local like = LIKE[name] or name
		return APPROVAL_RANK[like] or RANK[like] or 0
	end
	local function exceeds_ceiling(name, ceiling)
		if not (name and ceiling and RANK[name] and RANK[ceiling]) then
			return false
		end
		if RANK[name] > RANK[ceiling] then
			return true
		end
		return approval_rank_of(name) > approval_rank_of(ceiling)
	end

	--- switch preset. opts: {source = "user"|"cli"|"rpc"|"resume"|"init", confirm = bool,
	--- session = the session service to apply the sandbox through (restore: the active one)}
	local function set_preset(name, opts)
		opts = opts or {}
		if name == "yolo" then
			name = "full-access"
		end
		if not RANK[name] then
			return nil,
				"unknown preset: " .. tostring(name) .. " (" .. table.concat(PRESETS, " | ") .. ")",
				"permissions.unknown_preset"
		end
		-- sub-agents inherit the parent's preset and may only be stricter
		local ceiling = child and mare.args and mare.args.permission_ceiling
		if ceiling and exceeds_ceiling(name, ceiling) then
			return nil,
				("a sub-agent can't widen permissions beyond %s"):format(ceiling),
				"permissions.ceiling_exceeded"
		end
		-- first switch to full access (and any interactive switch in an untrusted folder): confirm
		if LIKE[name] == "full-access" and opts.confirm and LIKE[P.preset] ~= "full-access" then
			local st = mare.state.read() or {}
			if not st.full_access_confirmed or not mare.trusted then
				if not mare.ui.connected() then
					local why = mare.trusted and "full access needs a confirmation the first time"
						or "full access needs a confirmation in an untrusted folder"
					return nil, why, "permissions.needs_ui"
				end
				local prompt = mare.trusted
						and "Full access: commands run without a sandbox and nothing asks for approval. Continue?"
					or "Full access (untrusted folder): commands run without a sandbox and nothing asks for approval. Continue?"
				local ok = mare.ui.confirm(prompt, false)
				if not ok then
					return nil, "cancelled", "cancelled"
				end
				if mare.trusted and not st.full_access_confirmed then
					lib.save_state(ctx, "full_access_confirmed", true)
				end
			end
		end
		local ok, err = apply_sandbox(spec_for(name))
		if not ok then
			local err_s = tostring(err)
			-- When an active persistent terminal/job fences policy changes (`PTY_POLICY_BUSY`)
			-- and the previous host sandbox policy is confirmed still intact, keep `sandbox_ready`
			-- enabled instead of permanently shutting off requests.
			if
				P.sandbox_ready
				and err_s:find("PTY_POLICY_BUSY", 1, true)
				and err_s:find("previous host sandbox policy restored", 1, true)
			then
				return nil, err_s, "permissions.sandbox_failed"
			end
			P.sandbox_ready = false
			P.sandbox_error = "permission sandbox could not be restored: " .. err_s
			pcall(ctx.bus.emit, ctx.bus, "permission/changed", { preset = "off", label = "OFF" })
			return nil, P.sandbox_error, "permissions.sandbox_failed"
		end
		P.sandbox_ready = true
		P.sandbox_error = nil
		local prev = P.preset
		P.preset = name
		D.reset_session_grants()
		if opts.source ~= "resume" then
			record("preset", { preset = name, from = prev, source = opts.source })
		end
		emit()
		return name
	end

	local function validate_preset(name, label)
		if name == "yolo" then
			name = "full-access"
		end
		if type(name) ~= "string" or not RANK[name] then
			return nil, ("unknown %s: %s (%s)"):format(label, tostring(name), table.concat(PRESETS, " | "))
		end
		return name
	end

	local function explicit_cli_preset()
		if mare.config_cli then
			return mare.config_cli("plugins.permissions.preset")
		end
		return nil
	end

	--- the preset a new session starts with
	local function initial_preset()
		if cfg.untrusted_preset ~= nil then
			local _, uerr = validate_preset(cfg.untrusted_preset, "untrusted_preset")
			if uerr then
				return nil, uerr
			end
		end
		if cfg.preset ~= nil then
			local _, perr = validate_preset(cfg.preset, "preset")
			if perr then
				return nil, perr
			end
		end
		local explicit = explicit_cli_preset()
		if explicit ~= nil then
			local norm, err = validate_preset(explicit, "preset")
			if not norm then
				return nil, err
			end
			return norm, "cli"
		end
		if not child and not mare.trusted then
			local norm = validate_preset(cfg.untrusted_preset or "read-only", "untrusted_preset")
			return norm, "untrusted"
		end
		local norm = validate_preset(cfg.preset or "auto", "preset")
		return norm, "config"
	end

	return {
		RANK = RANK,
		LEVEL = LEVEL,
		LABEL = LABEL,
		DESC = DESC,
		PRESETS = PRESETS,
		LIKE = LIKE,
		win_default = win_default,
		with_common = with_common,
		spec_for = spec_for,
		base_spec = base_spec,
		apply_sandbox = apply_sandbox,
		same_policy = same_policy,
		is_partial = is_partial,
		is_network_partial = is_network_partial,
		emit = emit,
		set_preset = set_preset,
		explicit_cli_preset = explicit_cli_preset,
		initial_preset = initial_preset,
		exceeds_ceiling = exceeds_ceiling,
	}
end
