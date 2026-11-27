-- permissions.lifecycle: shell-tool escalation parameters, custom-sandbox detection, session
-- lifecycle and the first-launch notice; part of the `permissions` plugin (plugins/permissions.lua), loaded at the start of its apply.
return function(D)
	local ctx = D.ctx
	local P, lib, child, roots = D.P, D.lib, D.child, D.roots
	local Pr = D.presets
	local RANK, LEVEL, LABEL, DESC, exceeds_ceiling = Pr.RANK, Pr.LEVEL, Pr.LABEL, Pr.DESC, Pr.exceeds_ceiling
	local emit, set_preset, explicit_cli_preset, initial_preset, win_default =
		Pr.emit, Pr.set_preset, Pr.explicit_cli_preset, Pr.initial_preset, Pr.win_default
	local spec_for, apply_sandbox, same_policy = Pr.spec_for, Pr.apply_sandbox, Pr.same_policy

	---------------------------------------------------------------------------
	-- tool parameters for escalation requests (command tools)
	---------------------------------------------------------------------------
	-- Shadows the command tools' current definitions with the two parameters, through a `tools`
	-- follower: the shadows are that follower's effects, so they go with the tools service
	-- and come back with it. A shadow is revoked with the definition it wraps (a tool re-registered
	-- by a reload), so after each reload the current definitions are checked again; one that
	-- already carries both parameters (ours, or a later shadow copied from it) is left alone. Each
	-- parameter is added on its own: the vote reads them as a pair (vote.lua `scope, why`), so a
	-- replacement that declares one of them still gets the other.
	local function augment(tools)
		for _, def in ipairs(tools:list()) do
			if def and def.command_tool == true and not def.unconfined and type(def.parameters) == "table" then
				local name = def.name
				local props = def.parameters.properties or {}
				if props.sandbox_permissions == nil or props.justification == nil then
					def.parameters.properties = props
					props.sandbox_permissions = props.sandbox_permissions
						or {
							type = "string",
							-- default `use_default`; escalate only after a sandbox failure
							description = "Leave unset: commands run in the sandbox. Set it only to retry the exact "
								.. "command the sandbox just denied, with the narrowest access that suffices: "
								.. '"network:<host>", "write:<path>", or "network"; "all" (no sandbox) only when '
								.. "nothing narrower works. Reading files and writing inside the workspace never need "
								.. "it. The retry asks the user for approval.",
						}
					props.justification = props.justification
						or {
							type = "string",
							description = "Only together with sandbox_permissions: one short question for the user, "
								.. 'e.g. "Allow downloading packages from pypi.org?". Omit otherwise.',
						}
					tools:shadow(name, def)
				end
			end
		end
	end
	ctx:inject({ "tools" }, function(sub)
		augment(sub.tools)
		sub:on("kernel/reloaded", function()
			augment(sub.tools)
		end)
	end)

	---------------------------------------------------------------------------
	-- custom detection, session lifecycle
	---------------------------------------------------------------------------
	if mare.proc and mare.proc.sandbox and mare.proc.sandbox.generation then
		P.sandbox_gen = mare.proc.sandbox.generation()
	end
	function D.sync_external_sandbox()
		if P.applying or not (mare.proc and mare.proc.sandbox and mare.proc.sandbox.generation) then
			return
		end
		local gen = mare.proc.sandbox.generation()
		if P.sandbox_gen == nil then
			P.sandbox_gen = gen
			return
		end
		if gen == P.sandbox_gen then
			return
		end
		P.sandbox_gen = gen
		D.reset_session_grants()
		if P.preset ~= "custom" then
			local next_preset, approval_like = Pr.effective_preset_for_sandbox(P.preset)
			if next_preset ~= P.preset or approval_like ~= P.approval_like then
				P.preset = next_preset
				P.approval_like = approval_like
				emit()
			end
		elseif P.approval_like ~= nil then
			local pol = mare.proc.sandbox.current()
			if pol and pol.level == "read-only" then
				P.preset = "read-only"
				P.approval_like = nil
				emit()
			end
		end
	end
	ctx:on("sandbox/changed", function(ev)
		if P.applying then
			return
		end
		if mare.proc and mare.proc.sandbox and mare.proc.sandbox.generation then
			P.sandbox_gen = mare.proc.sandbox.generation()
		end
		-- A new host baseline revokes transient capabilities even when its level is unchanged.
		D.reset_session_grants()
		local pol = ev and ev.policy
		local level = pol and pol.level or "danger-full-access"
		if P.preset ~= "custom" and LEVEL[P.preset] ~= level then
			local _, approval_like = Pr.effective_preset_for_sandbox(P.preset)
			P.preset = "custom"
			P.approval_like = approval_like
			emit()
		end
	end)

	local TRUSTED_PRESET_SOURCES = {
		user = true,
		cli = true,
		api = true,
		option = true,
		config = true,
		untrusted = true,
		fallback = true,
		rpc = true,
	}

	local function warn_ui(msg)
		mare.log("warn", msg)
		local connected_ok, connected = pcall(mare.ui.connected)
		if connected_ok and connected then
			pcall(mare.ui.notify, msg, "warning")
		end
	end
	local function notify_cap(from, to, reason)
		if reason == "ceiling" then
			warn_ui(
				("Session load ceiling: capped restored permission preset from %s to %s"):format(
					tostring(from),
					tostring(to)
				)
			)
		else
			warn_ui(
				("Untrusted project: capped restored permission preset from %s to %s"):format(
					tostring(from),
					tostring(to)
				)
			)
		end
	end

	local function cap_preset(name, ceiling)
		if ceiling and RANK[ceiling] and exceeds_ceiling(name, ceiling) then
			return ceiling, true
		end
		return name, false
	end

	local function enforce_sb_ceiling(sb_ceiling)
		if type(sb_ceiling) ~= "table" then
			return true
		end
		local cur = mare.proc.sandbox.current()
		local cur_ok, cur_snap = pcall(mare.proc.sandbox.snapshot_current)
		if not cur_ok then
			return nil, "could not verify current host sandbox policy against ceiling: " .. tostring(cur_snap)
		end
		local ok_int, capped = pcall(mare.proc.sandbox.intersect, cur or { level = "danger-full-access" }, sb_ceiling)
		if not ok_int or capped == nil then
			local ceil_ok, ceil_norm = pcall(mare.proc.sandbox.snapshot_policy, sb_ceiling)
			if ceil_ok and same_policy(cur_snap, ceil_norm) then
				return true
			end
			return nil, "could not intersect current host sandbox policy with ceiling: " .. tostring(capped)
		end
		local cap_ok, cap_norm = pcall(mare.proc.sandbox.snapshot_policy, capped)
		if not cap_ok then
			return nil, "could not verify sandbox ceiling policy: " .. tostring(cap_norm)
		end
		if not same_policy(cur_snap, cap_norm) then
			local ok_sb, sb_err = apply_sandbox(capped)
			if not ok_sb then
				return nil, sb_err
			end
			if P.preset ~= "custom" and LEVEL[P.preset] ~= (capped.level or "danger-full-access") then
				P.preset = "custom"
				emit()
			end
		end
		return true
	end

	local function restore_from_session(perm_ceiling, sb_ceiling)
		-- An invalid permission configuration keeps the safe failure state: a preset saved in
		-- the session must not reopen tool execution over a configuration that failed validation.
		local valid, config_err = initial_preset()
		if not valid then
			return nil, config_err
		end
		if explicit_cli_preset() ~= nil then
			return false
		end
		local session = D.svc.session
		if not session then
			return false
		end
		local recs = require("mare_lib").all_records(session, "permission/preset", { branch = "current" }) or {}
		local last = recs[#recs]
		if last and RANK[last.preset] then
			if type(last.source) ~= "string" or not TRUSTED_PRESET_SOURCES[last.source] then
				mare.log("warn", "permissions: ignoring unverified permission/preset record on resume")
				return false
			end
			local target = last.preset
			local cap_reason
			if not mare.trusted and exceeds_ceiling(target, valid) then
				target = valid
				cap_reason = "untrusted"
			end
			local capped_target, was_capped = cap_preset(target, perm_ceiling)
			if was_capped then
				target = capped_target
				cap_reason = "ceiling"
			end
			local ok, err = set_preset(target, { source = "resume" })
			if not ok then
				return nil, err
			end
			local sb_ok, sb_err = enforce_sb_ceiling(sb_ceiling)
			if not sb_ok then
				return nil, sb_err
			end
			if target ~= last.preset then
				notify_cap(last.preset, target, cap_reason)
			end
			return true
		end
		return false
	end
	local function permission_failure(reason)
		P.sandbox_ready = false
		P.sandbox_error = tostring(reason or "permission sandbox could not be restored")
		pcall(ctx.bus.emit, ctx.bus, "permission/changed", { preset = "off", label = "OFF" })
		warn_ui(P.sandbox_error)
	end
	local function start()
		local perm_ceiling = mare.args and mare.args.restore_permission_ceiling or nil
		local sb_ceiling = mare.args and mare.args.restore_sandbox_ceiling or nil
		local default_name, default_source = initial_preset()
		if not default_name then
			permission_failure(default_source)
			return
		end
		local effective_default = cap_preset(default_name, perm_ceiling)
		local spec = spec_for(effective_default)
		local watching, watch_err = pcall(ctx.inject, ctx, { "session" }, function(sub)
			local session = sub.session
			if require("mare_lib").callable(session.set_default_sandbox_mode) then
				local captured, capture_err = pcall(function()
					return session:set_default_sandbox_mode(spec)
				end)
				if not captured then
					mare.log(
						"warn",
						"permissions: default sandbox baseline was not captured: " .. tostring(capture_err)
					)
				end
			end
		end)
		if not watching then
			mare.log("warn", "permissions: session sandbox baseline watcher failed: " .. tostring(watch_err))
		end
		local restored_ok, restored, restore_err = pcall(restore_from_session, perm_ceiling, sb_ceiling)
		if not restored_ok then
			permission_failure("permission sandbox could not be restored from the session: " .. tostring(restored))
			return
		elseif restore_err then
			permission_failure(restore_err)
			return
		elseif restored then
			return
		end
		local ok, err = set_preset(effective_default, { source = default_source })
		if not ok then
			local warning = "permissions: " .. tostring(err) .. "; using read-only fallback"
			warn_ui(warning)
			local safe, safe_err = set_preset("read-only", { source = "fallback" })
			if not safe then
				permission_failure(
					"permission sandbox could not be restored: "
						.. tostring(err)
						.. "; read-only fallback failed: "
						.. tostring(safe_err)
				)
				return
			end
		end
		local sb_ok, sb_err = enforce_sb_ceiling(sb_ceiling)
		if not sb_ok then
			permission_failure(sb_err)
		end
	end
	-- Restore this VM's previous sandbox policy when the plugin goes away (disable / reload /
	-- host shutdown); an inherited child ceiling still applies.
	local before = mare.proc.sandbox.current()
	ctx:effect(function()
		return function()
			pcall(mare.proc.sandbox.set_current, before or false)
		end
	end, "cleanup")
	local partial_notice_sent = false
	local function notice_partial()
		local current = mare.proc.sandbox.current()
		local missing_protection, accepted_protection = false, false
		for _, cap in ipairs(current and current.unavailable or {}) do
			missing_protection = missing_protection or cap == "fs_protect"
		end
		for _, cap in ipairs(current and current.accept_partial or {}) do
			accepted_protection = accepted_protection or cap == "fs_protect"
		end
		if partial_notice_sent or child or not missing_protection or not accepted_protection then
			return
		end
		partial_notice_sent = true
		local msg = "Sandbox file protection is PARTIAL on this host; command results identify unavailable protections"
		mare.log("warn", msg)
		if mare.ui.connected() then
			mare.ui.notify(msg, "warning")
		end
	end
	local function apply_initial(source_override, perm_ceiling, sb_ceiling)
		local name, src = initial_preset()
		if not name then
			permission_failure(src)
			return nil
		end
		name = cap_preset(name, perm_ceiling)
		local applied, apply_err = set_preset(name, { source = source_override or src })
		if not applied then
			permission_failure(apply_err)
			return nil
		end
		local sb_ok, sb_err = enforce_sb_ceiling(sb_ceiling)
		if not sb_ok then
			permission_failure(sb_err)
			return nil
		end
		return name
	end
	local WIN_DEFAULT_WARNING = "Network isn't sandboxed on this tier (PARTIAL): `mare sandbox setup`"
	-- first launch: one notice about the current preset and /permission; on the Windows
	-- default tier the network caveat joins it (one card, not a stack of first-run cards
	-- that pushes the conversation into the scrollback). Emitted pre-ready when the UI is
	-- already connected so the first frame matches post-ready cell-for-cell.
	local function emit_first_launch_notice()
		if not mare.ui.connected() or child then
			return
		end
		local st = mare.state.read() or {}
		local lines, level = {}, "info"
		if not st.permissions_intro_shown then
			lib.save_state(ctx, "permissions_intro_shown", true)
			lines[#lines + 1] = ("Permissions: %s (%s) — /permission to change"):format(
				LABEL[P.preset],
				DESC[P.preset] or "custom"
			)
		end
		if win_default() and not st.sandbox_setup_hint_shown then
			lib.save_state(ctx, "sandbox_setup_hint_shown", true)
			lines[#lines + 1] = WIN_DEFAULT_WARNING
			level = "warning"
		end
		if #lines > 0 then
			mare.ui.notify(table.concat(lines, "\n"), level)
		end
	end
	start()
	emit_first_launch_notice()
	ctx:on("session/changed", function()
		D.reset_session_grants()
	end)
	ctx:on("session/loaded", function(ev)
		D.reset_session_grants()
		local perm_ceiling = (ev and ev.restore_permission_ceiling)
			or (mare.args and mare.args.restore_permission_ceiling)
			or nil
		local sb_ceiling = (ev and ev.restore_sandbox_ceiling)
			or (mare.args and mare.args.restore_sandbox_ceiling)
			or nil
		if explicit_cli_preset() ~= nil then
			apply_initial(nil, perm_ceiling, sb_ceiling)
			partial_notice_sent = false
			notice_partial()
			return
		end
		local ok, restored, err = pcall(restore_from_session, perm_ceiling, sb_ceiling)
		if not ok then
			permission_failure("permission sandbox could not be restored from the session: " .. tostring(restored))
		elseif err then
			permission_failure(err)
		elseif not restored then
			local modes_ok, modes = pcall(function()
				return D.svc.session
						and require("mare_lib").all_records(D.svc.session, "sandbox/mode", { branch = "current" })
					or {}
			end)
			if not modes_ok then
				permission_failure("permission sandbox could not be restored from the session: " .. tostring(modes))
			elseif #modes == 0 then
				apply_initial("resume", perm_ceiling, sb_ceiling)
			elseif not mare.trusted or perm_ceiling ~= nil or type(sb_ceiling) == "table" then
				local last_mode = modes[#modes]
				-- Preserve a narrower saved read-only mode (including session-restored readable /
				-- deny_read restrictions) even when the ceiling is `auto`, intersecting with both
				-- the stock read-only spec and any active sandbox ceiling (narrowing network too).
				if type(last_mode) == "table" and last_mode.level == "read-only" then
					local ro_spec = spec_for("read-only")
					local cur = mare.proc.sandbox.current()
					local base = type(cur) == "table" and cur or last_mode
					local ok_int, target = pcall(mare.proc.sandbox.intersect, base, ro_spec)
					if not ok_int or target == nil then
						permission_failure(
							"permission sandbox could not be intersected with read-only preset: " .. tostring(target)
						)
					else
						if type(sb_ceiling) == "table" then
							local ok_cap, capped = pcall(mare.proc.sandbox.intersect, target, sb_ceiling)
							if not ok_cap or capped == nil then
								permission_failure(
									"permission sandbox could not be intersected with sandbox ceiling: "
										.. tostring(capped)
								)
								target = nil
							else
								target = capped
							end
						end
						if target ~= nil then
							local cur_ok, cur_snap = pcall(mare.proc.sandbox.snapshot_current)
							local tgt_ok, tgt_norm = pcall(mare.proc.sandbox.snapshot_policy, target)
							if not cur_ok then
								permission_failure(
									"could not verify current host sandbox policy: " .. tostring(cur_snap)
								)
							elseif not tgt_ok then
								permission_failure("could not verify read-only sandbox policy: " .. tostring(tgt_norm))
							else
								local applied, apply_err = true, nil
								if not same_policy(cur_snap, tgt_norm) then
									applied, apply_err = apply_sandbox(target)
								end
								if not applied then
									permission_failure(apply_err)
								else
									P.sandbox_ready = true
									P.sandbox_error = nil
									P.preset = "read-only"
									D.reset_session_grants()
									emit()
									local sb_ok, sb_err = enforce_sb_ceiling(sb_ceiling)
									if not sb_ok then
										permission_failure(sb_err)
									elseif last_mode.network == true and target.network ~= true then
										notify_cap(
											last_mode.level or "custom",
											"read-only",
											perm_ceiling and "ceiling" or "untrusted"
										)
									end
								end
							end
						end
					end
				else
					local capped = apply_initial("resume", perm_ceiling, sb_ceiling)
					local capped_spec = capped and spec_for(capped)
					if
						capped
						and type(last_mode) == "table"
						and (
							last_mode.level ~= LEVEL[capped]
							or (last_mode.network == true and capped_spec and capped_spec.network ~= true)
						)
					then
						notify_cap(last_mode.level or "custom", capped, perm_ceiling and "ceiling" or "untrusted")
					end
				end
			end
		end
		partial_notice_sent = false
		notice_partial()
	end)
	ctx:on("session/new", function(ev)
		D.reset_session_grants()
		local perm_ceiling = ev and ev.restore_permission_ceiling or nil
		local sb_ceiling = ev and ev.restore_sandbox_ceiling or nil
		apply_initial(nil, perm_ceiling, sb_ceiling)
		partial_notice_sent = false
		notice_partial()
	end)
	-- mare.Ctx still types `policy` as the transition layer's method
	local policy = ctx.policy --[[@as mare.PolicyService]]
	policy:voter("request/ready", function()
		if not P.sandbox_ready then
			return {
				decision = "deny",
				reason = P.sandbox_error or "permission sandbox could not be restored; request not sent",
			}
		end
	end)

	ctx:on("app/ready", function()
		notice_partial()
		if not mare.ui.connected() or child then
			return
		end
		emit_first_launch_notice()
		for _, msg in ipairs(roots.ready()) do
			mare.ui.notify(msg, "warning")
		end
	end)
end
