-- tools.pipeline_guard: admission, snapshot freezing, policy vote, approval, and re-check loop;
-- part of the `tools` plugin (plugins/tools.lua), loaded by tools.pipeline.
return function(D, projection)
	local ctx = D.ctx
	local tool_allowed = mare.subagent and mare.subagent.tool_allowed
	local mare_lib = require("mare_lib")
	local policy_binding = mare_lib.policy_binding
	local clone = D.results.clone
	local tools, registry = D.registry.tools, D.registry.registry
	local PROPAGATE = {}
	local policy_version, snapshot_hash = D.approval.policy_version, D.approval.snapshot_hash
	local approve, granted, denied_text = D.approval.approve, D.approval.granted, D.approval.denied_text
	local access = require("tools.access")(D)
	local function rule_signature(rule)
		local value = clone(rule)
		value.description = nil
		return mare.json.encode_strict(value)
	end
	local function rule_delta_matches(before, after, amendment, allow_unchanged)
		if type(before) ~= "table" or type(after) ~= "table" or type(amendment) ~= "table" then
			return false
		end
		local counts = {}
		for _, rule in ipairs(before) do
			local key = rule_signature(rule)
			counts[key] = (counts[key] or 0) + 1
		end
		for _, rule in ipairs(after) do
			local key = rule_signature(rule)
			counts[key] = (counts[key] or 0) - 1
		end
		local expected = rule_signature(amendment)
		local added_expected = false
		for key, count in pairs(counts) do
			if key == expected then
				if count == -1 then
					added_expected = true
				elseif count ~= 0 or not allow_unchanged then
					return false
				end
			elseif count ~= 0 then
				return false
			end
		end
		return added_expected or (allow_unchanged == true and (counts[expected] or 0) == 0)
	end
	local function without_rule_revision(version)
		local first, rest = tostring(version):match("^([^:]*):[^:]*:(.*)$")
		return first and (first .. ":" .. rest) or nil
	end
	local function same_non_rule_version(a, b)
		local left, right = without_rule_revision(a), without_rule_revision(b)
		return left ~= nil and left == right
	end

	local function call_cwd(d, arguments)
		if type(d.resolve_cwd) == "function" then
			return d.resolve_cwd(arguments)
		end
		return d.uses_shell_cwd and tools.cwd() or mare.cwd
	end

	local function definition_current(def)
		if def._removed then
			return false
		end
		local check = def.is_current or def._is_current
		if not check then
			return true
		end
		local ok, current = pcall(check)
		return ok and current == true
	end

	local function command_equal(a, b)
		if a == b then
			return true
		end
		if type(a) ~= "table" or type(b) ~= "table" then
			return false
		end
		if a.shell ~= b.shell or a.arg ~= b.arg then
			return false
		end
		local aa, ba = a.argv, b.argv
		if (aa == nil) ~= (ba == nil) then
			return false
		end
		if aa then
			if #aa ~= #ba then
				return false
			end
			for i = 1, #aa do
				if aa[i] ~= ba[i] then
					return false
				end
			end
		end
		local ad, bd = a.defaults, b.defaults
		if (ad == nil) ~= (bd == nil) then
			return false
		end
		if ad then
			for k, v in pairs(ad) do
				if bd[k] ~= v then
					return false
				end
			end
			for k in pairs(bd) do
				if ad[k] == nil then
					return false
				end
			end
		end
		return true
	end

	local function metadata_current(def, info)
		return info.side_effect == def.side_effect
			and info.idempotent == def.idempotent
			and info.unconfined == def.unconfined
			and info.permission_request == (def.permission_request == true)
			and info.command_tool == (def.command_tool == true)
			and command_equal(info.command, def.command)
	end

	local function admission_context_for(call, opts)
		local token = opts.declarations and opts.declarations[call.name] or nil
		local record = token and D.registry.resolve_binding(token) or nil
		local admit_model = opts.model or (record and record.model)
		local admit_mode = record and record.edit_mode
		local admit_opts = {
			step_id = opts.step_id ~= nil and opts.step_id or (record and record.step_id),
			session_id = record and record.session_id,
			session_generation = record and record.session_generation,
		}
		return record, admit_model, admit_mode, admit_opts
	end

	--- The guarded pipeline every tool call takes (rewrite → freeze snapshot → policy vote →
	--- approval → pre-exec hooks → controlled execution).
	---   1. `tool/rewrite` (strict chain) → schema validation;
	---   2. freeze a snapshot {arguments, cwd, sandbox requirement, policy version, tool
	---      definition version} + hash;
	---   3. `tool/before` votes on the snapshot (votes can't rewrite); `ask` → approval bound to
	---      the snapshot hash;
	---   4. right before running, re-check hash, cwd, policy version and tool definition; any
	---      change voids the approval and the call starts over at step 1 (at most 3 times).
	local function guarded(
		call,
		opts,
		def,
		info,
		run_cancel,
		execution_id,
		session,
		route_desc,
		route_snapshot,
		lock_slot
	)
		local access_declaration = def.access
		local function release_file_lock()
			if lock_slot and lock_slot.release then
				local release = lock_slot.release
				lock_slot.release = nil
				release()
			end
		end
		if tool_allowed and not tool_allowed(call.name) then
			return {
				content = "Tool call blocked by the inherited subagent tool ceiling",
				is_error = true,
				refused = true,
			}
		end
		if D.registry.excluded(call.name) then
			return { content = "Unknown tool: " .. tostring(call.name), is_error = true, refused = true }
		end
		if registry[call.name] ~= def or not definition_current(def) then
			return { content = "Tool call blocked: tool definition changed", is_error = true, refused = true }
		end
		local record, admit_model, admit_mode, admit_opts = admission_context_for(call, opts)
		if opts.declarations and (not record or record.definition ~= def or record.version ~= def._version) then
			return {
				content = "Tool call blocked: tool was not declared in this request or its definition changed",
				is_error = true,
				refused = true,
			}
		end
		if not D.registry.is_admitted(def, admit_model, admit_mode, admit_opts) then
			return { content = "Tool call blocked: tool is not admitted", is_error = true, refused = true }
		end
		local schema = record and record.schema or def._schema
		if not schema then
			return {
				content = "Tool call blocked: dynamic schema requires a request declaration",
				is_error = true,
				refused = true,
			}
		end
		local schema_fingerprint = mare.json.encode_strict(mare.projection.project(schema:value()))
		-- freeze by value: nobody holding the caller's table (or the voted copy) can change what runs
		local args = call.arguments
		local pristine = clone(args)
		local requested = opts.sandbox
		for attempt = 1, 3 do
			release_file_lock()
			do
				if registry[call.name] ~= def or not definition_current(def) then
					return { content = "Tool call blocked: tool definition changed", is_error = true, refused = true }
				end
				local rw_ok, rw_args = pcall(
					D.policy.rewrite,
					D.policy,
					"tool/rewrite",
					{ id = call.id, name = call.name, arguments = clone(pristine) }
				)
				if not rw_ok then
					if type(rw_args) == "table" and rw_args.code == "policy.rewrite_refused" then
						local reason = rw_args.message or tostring(rw_args)
						return { content = "Tool call blocked: " .. tostring(reason), is_error = true, refused = true }
					end
					error({ [PROPAGATE] = true, err = rw_args }, 0)
				end
				local rewritten = mare.projection.project(rw_args)
				local protected = mare.projection.contains(rewritten)
				local permitted, reason = projection.check(rewritten, def.sensitive_inputs)
				if not permitted then
					return { content = reason, is_error = true, refused = true }
				end

				local valid, verr = pcall(schema.validate_projected, schema, rewritten, mare.projection.revision())
				if not valid then
					return { content = "Invalid arguments: " .. tostring(verr), is_error = true, refused = true }
				end
				local frozen_access = access.evaluate(def, rewritten, call_cwd(def, rewritten))
				if
					lock_slot
					and frozen_access.state == "declared"
					and D.file_guard
					and D.file_guard.acquire_pipeline_lock
				then
					local write_paths = {}
					for _, entry in ipairs(frozen_access.entries or {}) do
						if entry.op == "write" or entry.op == "create" or entry.op == "delete" then
							write_paths[#write_paths + 1] = entry.logical
							if entry.physical and entry.physical ~= entry.logical then
								write_paths[#write_paths + 1] = entry.physical
							end
						end
					end
					if #write_paths > 0 then
						local rel, lock_err = D.file_guard.acquire_pipeline_lock(write_paths, opts.cancel)
						if not rel then
							if
								lock_err == "cancelled"
								or (opts.cancel and opts.cancel:is_cancelled())
								or run_cancel:is_cancelled()
							then
								return { content = "Tool call cancelled", is_error = true, cancelled = true }
							end
							error(lock_err, 0)
						end
						lock_slot.release = rel
						-- Re-evaluate once holding the per-file lock so frozen path identities
						-- reflect any preceding queued mutation on the same file.
						frozen_access = access.evaluate(def, rewritten, call_cwd(def, rewritten))
					end
				end
				local permissions = D.svc.permissions
				local initial_rules = permissions and permissions.rules and clone(permissions:rules()) or nil
				local accepted_rule_version
				info.access = frozen_access.entries and clone(frozen_access.entries) or nil
				info.access_state = frozen_access.state
				info.access_version = def._version
				info.tool_version = def._version
				local snap = {
					id = call.id,
					name = call.name,
					arguments = clone(rewritten), -- what runs: never handed to listeners
					cwd = call_cwd(def, rewritten),
					sandbox = requested,
					policy_version = policy_version(),
					tool_version = def._version,
					schema = schema_fingerprint,
					projection_version = mare.projection.revision(),
					input_policy = clone(def.sensitive_inputs or {}),
					access = frozen_access,
				}
				snap.hash = snapshot_hash(snap)
				local function stable_access_key(acc)
					if type(acc) ~= "table" or acc.state ~= "declared" then
						return acc
					end
					local entries = mare.json.array()
					for i, item in ipairs(acc.entries or {}) do
						entries[i] = {
							kind = item.kind,
							op = item.op,
							path = item.path,
							logical = item.logical,
							physical = item.physical,
							recursive = item.recursive,
						}
					end
					return { state = acc.state, entries = entries }
				end
				-- the same call independent of policy changes: a sub-agent's approved escalation
				-- (answered by the parent) is recorded against it by the host
				local approval_key = snapshot_hash({
					name = snap.name,
					arguments = snap.arguments,
					cwd = snap.cwd,
					sandbox = snap.sandbox,
					policy_version = "",
					tool_version = snap.tool_version,
					schema = schema_fingerprint,
					projection_version = snap.projection_version,
					input_policy = snap.input_policy,
					access = stable_access_key(snap.access),
				})
				-- the frozen call: one table for the vote, the approval and its result event
				local frozen = {
					id = call.id,
					execution_id = execution_id,
					name = call.name,
					arguments = clone(snap.arguments),
					snapshot = snap.hash,
					attempt = attempt,
					approval_key = approval_key,
					schema = schema:value(),
					projection_version = snap.projection_version,
					protected_arguments = protected,
					cwd = snap.cwd, -- permission rules with a `cwd` limit check this directory
					access = clone(snap.access),
				}
				if registry[call.name] ~= def or not definition_current(def) then
					return { content = "Tool call blocked: tool definition changed", is_error = true, refused = true },
						frozen
				end
				local function frame_current()
					return frozen.id == snap.id
						and call.id == snap.id
						and frozen.name == snap.name
						and frozen.cwd == snap.cwd
						and frozen.snapshot == snap.hash
						and frozen.approval_key == approval_key
						and frozen.protected_arguments == protected
						and mare.json.encode_strict(frozen.access) == mare.json.encode_strict(snap.access)
				end
				local function snapshot_current(amendment, after_decided)
					local permissions = D.svc.permissions
					local current_rules = permissions and permissions.rules and permissions:rules() or nil
					local current_version = policy_version()
					if current_version ~= snap.policy_version then
						-- A parent (for a child) or this session's `allow_amend` persistence may
						-- have added only the approved amendment rule. Accept only that delta and
						-- only if every other policy generation is unchanged.
						local allow_child = amendment
							and mare.mode == "child"
							and snap.access.state == "unknown"
							and info.command_tool
							and same_non_rule_version(current_version, snap.policy_version)
							and rule_delta_matches(initial_rules, current_rules, amendment, false)
						local allow_self = after_decided == true
							and amendment
							and mare.mode ~= "child"
							and same_non_rule_version(current_version, snap.policy_version)
							and rule_delta_matches(initial_rules, current_rules, amendment, true)
						if allow_child or allow_self then
							accepted_rule_version = current_version
						end
						if accepted_rule_version == current_version then
							current_version = snap.policy_version
						end
					end
					local cur = {
						name = call.name,
						projection_version = mare.projection.revision(),
						input_policy = def.sensitive_inputs or {},
						arguments = frozen.arguments,
						cwd = call_cwd(def, frozen.arguments),
						sandbox = requested,
						policy_version = current_version,
						tool_version = registry[call.name] and registry[call.name]._version,
						schema = schema_fingerprint,
						access = access.evaluate(def, frozen.arguments, call_cwd(def, frozen.arguments)),
					}
					return snapshot_hash(cur) == snap.hash
						and registry[call.name] == def
						and definition_current(def)
						and metadata_current(def, info)
						and def.access == access_declaration
						and frame_current()
				end
				---@type table<string, any>
				local snap_args = snap.arguments
				local expected_binding
				if info.permission_request or snap_args.additional_permissions ~= nil then
					expected_binding = policy_binding({
						id = snap.id,
						name = snap.name,
						cwd = snap.cwd,
						arguments = snap.arguments,
						snapshot = snap.hash,
						approval_key = approval_key,
						access = snap.access,
					}, mare_lib.tool_policy_info(info))
				end
				local before_info = table.copy(info)
				before_info.access = info.access and clone(info.access) or nil
				before_info.model = admit_model
				before_info.step_id = admit_opts.step_id
				before_info.execution_id = execution_id
				before_info.session_id = session.session_id
				before_info.session_generation = session.session_epoch
				before_info.snapshot = snap.hash
				before_info.attempt = attempt
				before_info.tool_call_id = call.id
				-- the exact registered definition: approvals given for it never carry over to a
				-- replacement of the same name (another provider's implementation)
				before_info.tool_version = def._version
				before_info.cwd = snap.cwd
				before_info.route = route_desc and table.copy(route_desc) or nil
				before_info.route_snapshot = route_snapshot
				local verdict = D.policy:decide("tool/before", table.pack(frozen, before_info))
				if not metadata_current(def, info) or not frame_current() then
					return {
						content = "Tool call blocked: tool metadata changed during policy evaluation",
						is_error = true,
						refused = true,
					},
						frozen
				end
				-- Literal rule matches cannot authorize an unseen substitution. A deny still wins.
				if protected and verdict.decision ~= "deny" then
					verdict.decision = "ask"
					verdict.reason = (verdict.reason and (verdict.reason .. "; ") or "")
						.. "Registered hidden values will be restored; explicit approval is required for this call"
					verdict.amendment = nil
				end
				if registry[call.name] ~= def or not definition_current(def) then
					return { content = "Tool call blocked: tool definition changed", is_error = true, refused = true },
						frozen
				end
				-- Approval callbacks see `verdict`; keep the effective grant detached from it.
				local sandbox = verdict.sandbox_grant ~= nil and clone(verdict.sandbox_grant) or nil
				for _, constraint in ipairs(verdict.sandboxes or {}) do
					sandbox = mare.proc.sandbox.intersect(sandbox, constraint) or sandbox
				end
				if requested ~= nil then
					-- a caller can only narrow what the vote decided (nil = the session's current
					-- policy); "off" never wins an intersection
					sandbox = mare.proc.sandbox.intersect(sandbox, requested) or sandbox
				end
				local function emit_approval_resolved(approval_id, call_tbl, event)
					if approval_id then
						local public_event = clone(event)
						public_event.call = { tool_call_id = call_tbl.id, name = call_tbl.name }
						ctx.bus:emit("approval/resolved", {
							approval_id = approval_id,
							category = "tool",
							tool_call_id = call_tbl.id,
							result = mare.approvals.public_result(public_event),
						})
					end
				end
				local permission_constraints, permission_scope, effective_permissions, approval_constraints, approval_sandbox, projection_authorization, post_amendment
				if verdict.decision == "ask" then
					local req_opts = { emit_resolved = false }
					-- Pair hard task cancellations (when pcall never resumes) with a fallback resolved.
					local resolved_guard <close> = setmetatable({}, {
						__close = function(_, err)
							if req_opts.requested and req_opts.approval_id and not req_opts.resolved then
								req_opts.resolved = true
								local cancelled = (opts.cancel and opts.cancel:is_cancelled()) or err == nil
								local fallback = {
									call = frozen,
									decision = "deny",
									source = cancelled and "cancelled" or "error",
									outcome = cancelled and "cancelled" or nil,
								}
								pcall(ctx.spawn, ctx, function()
									emit_approval_resolved(req_opts.approval_id, frozen, fallback)
								end)
							end
						end,
					})
					local ask_ok, ask_status, ask_v1, ask_v2 = pcall(function()
						local service = D.svc.permissions
						if service and service.describe_request then
							verdict.requested_permissions, verdict.permission_scope =
								service:describe_request(frozen, expected_binding)
						end
						local approval_verdict = verdict
						if opts.origin == "call_tool" then
							approval_verdict = clone(verdict)
							local base = type(approval_verdict.reason) == "string"
									and approval_verdict.reason ~= ""
									and approval_verdict.reason
								or nil
							approval_verdict.reason = base and (base .. " (via call_tool)") or "via call_tool"
						end
						local a, human_receipt, approval_id = approve(
							frozen,
							clone(info),
							approval_verdict,
							opts.approve,
							opts.cancel,
							snap.policy_version,
							schema:value(),
							req_opts
						)
						if registry[call.name] ~= def or not definition_current(def) then
							req_opts.resolved = true
							emit_approval_resolved(approval_id, frozen, {
								call = frozen,
								decision = "deny",
								source = "policy",
							})
							return "return",
								{
									content = "Tool call blocked: declared tool definition changed",
									is_error = true,
									refused = true,
								},
								frozen
						end
						if
							a.decision ~= "retry"
							and policy_version() ~= snap.policy_version
							and granted(a.decision)
						then
							local recheck = D.policy:decide(
								"tool/approval",
								table.pack(clone(frozen), clone(info), clone(verdict), opts.cancel)
							)
							if recheck.decision == "deny" then
								a = {
									decision = recheck.abort and "abort" or "deny",
									reason = recheck.reason or "denied by policy",
									source = recheck.source or "policy",
								}
							else
								a = { decision = "retry", voided = true }
							end
						end
						-- Every answer, including opts.approve and child replies, meets here. Source
						-- labels are display data; only native evidence can authorize hidden inputs.
						local human, authorization
						if human_receipt and human_receipt.binding.snapshot == snap.hash then
							human, authorization = mare.approvals.consume_human(
								human_receipt.token,
								human_receipt.binding,
								snap.arguments,
								protected and snap.projection_version or nil,
								human_receipt.decision
							)
						end
						if granted(a.decision) and protected then
							if not human or not authorization then
								a = {
									decision = "deny",
									reason = "hidden values require human approval for this exact action",
									source = "approval_policy",
								}
							else
								projection_authorization = authorization
								a.decision = "allow"
								a.amendment = nil
								if a.scope == "session" then
									a.scope = "turn"
								end
							end
						end
						if a.source == "user" and not human then
							a.source = "policy"
						end
						if a.decision == "retry" then
							req_opts.resolved = true
							emit_approval_resolved(approval_id, frozen, {
								call = frozen,
								decision = "deny",
								source = "policy",
							})
							if a.voided then
								-- an approval was given under the old policy: it is void, start over
								ctx.bus:emit("tool/snapshot-void", {
									call = { id = frozen.id, name = frozen.name },
									reason = "the permission policy changed before it ran",
								})
							end
							return "retry"
						end
						if a.unavailable ~= nil then
							assert(type(a.unavailable) == "boolean", "invalid approval unavailable flag")
							if a.unavailable then
								assert(not granted(a.decision), "contradictory approval unavailable flag")
								a.decision = "unavailable"
							end
						end
						if a.abort ~= nil then
							assert(type(a.abort) == "boolean", "invalid approval abort flag")
							if a.abort then
								assert(not granted(a.decision), "contradictory approval abort flag")
								a.decision = "abort"
							end
						end
						if a.grant ~= nil then
							assert(a.decision == "allow" and granted(a.grant), "invalid approval grant")
							a.decision = a.grant
						end
						if snap_args.additional_permissions ~= nil or snap_args.sandbox_permissions ~= nil then
							local fields = {
								decision = true,
								grant = true,
								abort = true,
								unavailable = true,
								reason = true,
								source = true,
								amendment = true,
								sandbox = true,
								sandbox_constraints = true,
								permissions = true,
								permission_constraints = true,
								scope = true,
								outcome = true,
							}
							for key in pairs(a) do
								assert(fields[key], "unknown permission approval field: " .. tostring(key))
							end
						end
						approval_constraints = a.sandbox_constraints and clone(a.sandbox_constraints) or nil
						approval_sandbox = a.sandbox ~= nil and clone(a.sandbox) or nil
						permission_constraints = a.permission_constraints and clone(a.permission_constraints) or nil
						if a.permissions then
							permission_constraints = permission_constraints or {}
							table.insert(permission_constraints, clone(a.permissions))
						end
						permission_scope = a.scope
							or ((a.decision == "allow_session" or a.decision == "allow_amend") and "session" or nil)
							or (
								snap_args.additional_permissions ~= nil
									and (info.permission_request or snap_args.scope ~= nil)
									and "turn"
								or nil
							)
						local permission_service = D.svc.permissions
						local constrained_permissions = a.permissions ~= nil
							or (
								a.permission_constraints ~= nil
								and (type(a.permission_constraints) ~= "table" or next(a.permission_constraints) ~= nil)
							)
						assert(
							not (constrained_permissions and def.unconfined),
							"partial permission constraints cannot confine an unconfined tool"
						)
						if constrained_permissions or info.permission_request then
							assert(
								permission_service and permission_service.resolve_request,
								"permission resolver unavailable; refusing structured grant"
							)
						end
						if granted(a.decision) and permission_service and permission_service.resolve_request then
							local _sandbox, resolved_scope
							effective_permissions, _sandbox, resolved_scope = permission_service:resolve_request(
								frozen,
								permission_constraints,
								permission_scope,
								expected_binding
							)
							if effective_permissions then
								permission_scope = resolved_scope
							end
							if constrained_permissions or info.permission_request then
								assert(
									effective_permissions,
									"permission approval requires a frozen permission request"
								)
							end
						end
						if
							(
								permission_scope == "turn"
								and (a.decision == "allow_session" or a.decision == "allow_amend")
							) or (protected and granted(a.decision))
						then
							a.decision, a.amendment = "allow", nil
						end
						-- No reusable approval or persistent rule is recorded for a snapshot whose
						-- path identity, declaration or policy changed during the answer.
						if
							granted(a.decision)
							and not snapshot_current(a.decision == "allow_amend" and a.amendment or nil)
						then
							req_opts.resolved = true
							emit_approval_resolved(
								approval_id,
								frozen,
								{ call = frozen, decision = "deny", source = "policy" }
							)
							ctx.bus:emit("tool/snapshot-void", {
								call = { id = frozen.id, name = frozen.name },
								reason = "the call changed before it ran",
							})
							return "retry"
						end
						local approval_event = {
							call = frozen,
							info = info,
							decision = a.decision,
							outcome = a.outcome,
							amendment = a.amendment,
							source = a.source,
							reason = a.reason,
							snapshot = snap.hash,
							permissions = (effective_permissions or a.permissions) and clone(
								effective_permissions or a.permissions
							) or nil,
							permission_constraints = permission_constraints and clone(permission_constraints) or nil,
							scope = permission_scope,
						}
						ctx.bus:emit("pipeline/approval-decided", approval_event)
						req_opts.resolved = true
						emit_approval_resolved(approval_id, frozen, approval_event)
						return "done", a
					end)
					if not ask_ok then
						if req_opts.requested and req_opts.approval_id and not req_opts.resolved then
							req_opts.resolved = true
							emit_approval_resolved(req_opts.approval_id, frozen, {
								call = frozen,
								decision = "deny",
								source = "error",
							})
						end
						error(ask_status)
					end
					if ask_status == "return" then
						return ask_v1, ask_v2
					elseif ask_status == "retry" then
						goto retry
					end
					local a = ask_v1
					if a.decision == "unavailable" then
						local session = D.svc.session
						if session then
							pcall(session.add_record, session, "approval/unavailable", {
								tool_call_id = call.id,
								tool = call.name,
								arguments = snap.arguments,
								reason = a.reason,
							})
						end
					end
					if a.decision == "abort" and (run_cancel or opts.cancel) then
						(run_cancel or opts.cancel):cancel()
					end
					if not granted(a.decision) then
						return {
							content = denied_text(a.reason or verdict.reason),
							is_error = true,
							refused = true,
						},
							frozen
					end
					if approval_sandbox ~= nil then
						sandbox = mare.proc.sandbox.intersect(sandbox, approval_sandbox) or sandbox
					end
					for _, constraint in ipairs(approval_constraints or {}) do
						sandbox = mare.proc.sandbox.intersect(sandbox, constraint) or sandbox
					end
					if a.decision == "allow_amend" and type(a.amendment) == "table" and mare.mode ~= "child" then
						post_amendment = clone(a.amendment)
						if post_amendment.expires == "session" and post_amendment.tool_version == nil then
							post_amendment.tool_version = tostring(info and info.tool_version or "")
						end
					end
				elseif verdict.decision ~= "allow" then
					if verdict.abort and (run_cancel or opts.cancel) then
						(run_cancel or opts.cancel):cancel()
					end
					return {
						content = "Tool call blocked: " .. tostring(verdict.reason or "denied by policy"),
						is_error = true,
						refused = true,
					},
						frozen
				elseif sandbox ~= nil and mare.subagent and mare.subagent.rule_lift then
					-- a sub-agent: a call its parent's rules allow escalates past the inherited
					-- ceiling, like the main agent's
					mare.subagent.rule_lift(frozen, info)
				end
				local permissions = D.svc.permissions
				if info.permission_request then
					assert(
						permissions and permissions.resolve_request,
						"permission resolver unavailable; refusing structured grant"
					)
				end
				if permissions and permissions.resolve_request then
					local narrowed
					effective_permissions, narrowed, permission_scope =
						permissions:resolve_request(frozen, permission_constraints, permission_scope, expected_binding)
					if info.permission_request then
						assert(effective_permissions, "permission grant requires a frozen permission request")
					end
					if effective_permissions then
						sandbox = narrowed
						for _, constraint in ipairs(verdict.sandboxes or {}) do
							sandbox = mare.proc.sandbox.intersect(sandbox, constraint) or sandbox
						end
						if approval_sandbox ~= nil then
							sandbox = mare.proc.sandbox.intersect(sandbox, approval_sandbox) or sandbox
						end
						for _, constraint in ipairs(approval_constraints or {}) do
							sandbox = mare.proc.sandbox.intersect(sandbox, constraint) or sandbox
						end
						if requested ~= nil then
							sandbox = mare.proc.sandbox.intersect(sandbox, requested) or sandbox
						end
					end
				end
				-- re-check right before running: an approval binds this exact snapshot
				if snapshot_current(post_amendment, post_amendment ~= nil) then
					if opts.cancel and opts.cancel:is_cancelled() then
						return { content = "Tool call cancelled", is_error = true, refused = true }, frozen
					end
					-- Narrowing caps imposed on this call (caller `opts.sandbox`, outer voter
					-- `verdict.sandboxes`, approval constraints) without baking in an outer
					-- dispatcher's own `verdict.sandbox_grant` baseline.
					local sandbox_cap
					local function add_cap(cap)
						if cap == nil then
							return
						end
						if sandbox_cap == nil then
							sandbox_cap = clone(cap)
						else
							sandbox_cap = mare.proc.sandbox.intersect(sandbox_cap, cap) or sandbox_cap
						end
					end
					for _, constraint in ipairs(verdict.sandboxes or {}) do
						add_cap(constraint)
					end
					add_cap(approval_sandbox)
					for _, constraint in ipairs(approval_constraints or {}) do
						add_cap(constraint)
					end
					add_cap(requested)
					return nil,
						frozen,
						sandbox,
						snap.arguments,
						snap.hash,
						approval_key,
						schema_fingerprint,
						effective_permissions,
						permission_scope,
						admit_model,
						admit_opts,
						projection_authorization,
						snap.policy_version,
						accepted_rule_version,
						sandbox_cap
				end
				if registry[call.name] ~= def or not definition_current(def) then
					return {
						content = "Tool call blocked: declared tool definition changed",
						is_error = true,
						refused = true,
					},
						frozen
				end
				-- something changed while voting / waiting for approval: start over from the rewrite
				ctx.bus:emit(
					"tool/snapshot-void",
					{ call = { id = frozen.id, name = frozen.name }, reason = "the call changed before it ran" }
				)
			end
			::retry::
		end
		return {
			content = "Tool call blocked: its arguments, working directory or permission policy kept changing before it could run",
			is_error = true,
			refused = true,
		}
	end

	return {
		PROPAGATE = PROPAGATE,
		access = access,
		call_cwd = call_cwd,
		definition_current = definition_current,
		metadata_current = metadata_current,
		guarded = guarded,
	}
end
