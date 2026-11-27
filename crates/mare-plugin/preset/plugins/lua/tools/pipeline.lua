-- tools.pipeline: validation, result truncation and the guarded execution pipeline
-- (tools.execute); part of the `tools` plugin (plugins/tools.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local protected_call = pcall
	local projection = require("tools.projection")
	projection.observe(ctx)
	local clone, coded_failure = D.results.clone, D.results.coded_failure
	local tools, registry = D.registry.tools, D.registry.registry
	local policy_version, snapshot_hash = D.approval.policy_version, D.approval.snapshot_hash
	local guard = require("tools.pipeline_guard")(D, projection)
	local call_cwd, definition_current, metadata_current, guarded =
		guard.call_cwd, guard.definition_current, guard.metadata_current, guard.guarded

	--- execute(call, opts) -> result, frozen_call. The built-in tools service's entry: rewrite,
	--- policy vote, approval, snapshot re-check, persistence hooks and controlled execution all
	--- happen here. opts:
	---   cancel (token) · on_update(partial) · sandbox (can only narrow the voted policy) ·
	---   on_state(call, state, info) -> ok, err: persistence of `prepared` / `started` (a
	---     failure refuses the call with error_code = "persist_failed") ·
	---   approve(call, info, verdict) -> {decision,...}: replaces the built-in approval UI.
	--- The tool runs inside a core execution context: every mare.fs / proc / http / net call
	--- it makes is checked against the sandbox policy.
	function tools.execute(_, _, call, opts)
		opts = table.copy(opts or {})
		local execution_id = mare.crypto.uuid()
		local route_ok, route_or_err, route_snapshot = D.results.validate_route_and_state(opts, call)
		if not route_ok then
			return route_or_err, call
		end
		local route_desc = route_or_err
		local session_svc = D.svc.session
		local session = session_svc and session_svc:info() or {}
		local retention_max_bytes = cfg.max_bytes
		local llm_svc = D.svc.llm
		local model = llm_svc and llm_svc:current()
		local model_policy = model and model.truncation
		if model_policy and model_policy.limit then
			retention_max_bytes = math.min(
				retention_max_bytes,
				model_policy.mode == "tokens" and model_policy.limit * 4 or model_policy.limit
			)
		end
		local supplied_cancel = opts.cancel
		local tok = supplied_cancel and supplied_cancel:child() or mare.task.token()
		local run_cancel = opts.run_cancel or supplied_cancel or tok
		opts.cancel = tok
		local scope_watcher
		if opts.run_cancel then
			scope_watcher = mare.task.spawn(function()
				run_cancel:wait()
				tok:cancel()
			end)
		end
		local running
		local _invocation_guard <close> = setmetatable({}, {
			__close = function()
				if scope_watcher then
					scope_watcher:abort()
				end
				tok:cancel()
				if running then
					running[tok] = nil
				end
			end,
		})
		local def = registry[call.name]
		if not def then
			return { content = "Unknown tool: " .. tostring(call.name), is_error = true }
		end
		if D.registry.excluded(call.name) then
			return { content = "Unknown tool: " .. tostring(call.name), is_error = true, refused = true }
		end
		-- a plugin reload in progress finishes first (the tool may be being replaced)
		ctx.kernel:reload_barrier()
		def = registry[call.name]
		if not def then
			return { content = "Unknown tool: " .. tostring(call.name), is_error = true }
		end
		if D.registry.excluded(call.name) then
			return { content = "Unknown tool: " .. tostring(call.name), is_error = true, refused = true }
		end
		-- per-call token: track from guarded execution (including pending approval waits)
		-- so revoking/unregistering the tool cancels pending approvals immediately
		running = def._running
		if running then
			running[tok] = true
		end
		local info = require("mare_lib").tool_policy_info(def)
		local function persisted(state, c)
			return D.results.persist_state(opts.on_state, info, state, c)
		end
		local pok, perr = persisted("prepared", call)
		if not pok then
			return perr, call
		end
		local lock_slot = {}
		local function release_file_lock()
			local release = lock_slot.release
			if release then
				lock_slot.release = nil
				release()
			end
		end
		local _file_lock_guard <close> = setmetatable({}, { __close = release_file_lock })
		local ok, refused, frozen, sandbox, args, approved_hash, approval_key, schema_fingerprint, effective_permissions, permission_scope, admit_model, admit_opts, projection_authorization, approved_policy_version, accepted_rule_version, sandbox_cap =
			pcall(
				guarded,
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
		if not ok then
			if type(refused) == "table" and refused[guard.PROPAGATE] then
				error(refused.err, 0)
			end
			return { content = "Invalid arguments: " .. tostring(refused), is_error = true, refused = true }, call
		end
		if refused then
			return refused, frozen or call
		end
		if not frozen then
			return { content = "Tool call blocked: missing frozen call state", is_error = true, refused = true }, call
		end
		if opts.cancel:is_cancelled() or run_cancel:is_cancelled() then
			return { content = "Tool call cancelled", is_error = true, cancelled = true }, frozen
		end
		def = registry[call.name]
		local execute = def.execute
		if type(execute) ~= "function" then
			return { content = "Tool call blocked: tool implementation is unavailable", is_error = true, refused = true },
				frozen
		end
		-- `started` must be on disk before the call runs (crash recovery must know it may have run)
		local state_ok, sok, serr = pcall(persisted, "started", frozen)
		if not state_ok then
			error(sok)
		end
		if not sok then
			return serr, frozen
		end
		local approved_id, approved_name, approved_cwd = frozen.id, frozen.name, frozen.cwd
		local approved_access = def.access
		local approved_command = def.command
		local function approval_current()
			local access_ok, current_access =
				pcall(guard.access.evaluate, def, frozen.arguments, call_cwd(def, frozen.arguments))
			if not access_ok then
				return false
			end
			local cmd_ok, current_command =
				pcall(guard.command.evaluate, def, frozen.arguments, call_cwd(def, frozen.arguments))
			if not cmd_ok then
				return false
			end
			local current_policy_version = policy_version()
			if accepted_rule_version and current_policy_version == accepted_rule_version then
				current_policy_version = approved_policy_version
			end
			return registry[call.name] == def
				and def.access == approved_access
				and def.command == approved_command
				and mare.json.encode_strict(frozen.access) == mare.json.encode_strict(current_access)
				and frozen.id == approved_id
				and call.id == approved_id
				and frozen.name == approved_name
				and frozen.cwd == approved_cwd
				and frozen.snapshot == approved_hash
				and frozen.approval_key == approval_key
				and metadata_current(def, info)
				and definition_current(def)
				and def.execute == execute
				and snapshot_hash({
						name = call.name,
						arguments = frozen.arguments,
						cwd = call_cwd(def, frozen.arguments),
						sandbox = opts.sandbox,
						policy_version = current_policy_version,
						tool_version = registry[call.name] and registry[call.name]._version,
						schema = schema_fingerprint,
						projection_version = mare.projection.revision(),
						input_policy = def.sensitive_inputs or {},
						access = current_access,
						command = current_command,
					})
					== approved_hash
		end
		local function changed_refusal()
			return {
				content = "Tool call blocked: its approval changed before execution",
				is_error = true,
				refused = true,
			}
		end
		if not approval_current() then
			return changed_refusal(), frozen
		end
		if mare.subagent and mare.subagent.permission_lift then
			mare.subagent.permission_lift(frozen)
		end
		local declared_tools
		if opts.declarations then
			declared_tools = mare.json.array()
			for name in pairs(opts.declarations) do
				declared_tools[#declared_tools + 1] = name
			end
			table.sort(declared_tools)
		end
		local tctx = {
			cancel = tok,
			run_cancel = run_cancel,
			update = opts.on_update,
			cwd = frozen.cwd,
			tool_call_id = call.id,
			execution_id = execution_id,
			idempotency_key = call.id,
			permissions = effective_permissions,
			permission_scope = permission_scope,
			model = admit_model,
			step_id = admit_opts.step_id,
			declared_tools = declared_tools,
		}
		local close_nested = D.nested.attach(tctx, call, {
			declarations = opts.declarations,
			route_snapshot = route_snapshot,
			model = admit_model,
			step_id = admit_opts.step_id,
			approve = opts.approve,
			lifetime = opts.lifetime,
			sandbox = sandbox_cap,
			on_output = opts.audience ~= "program" and type(opts.on_output) == "function" and opts.on_output or nil,
			execution_id = execution_id,
		})
		local _nested_guard <close> = setmetatable({}, { __close = close_nested })
		local timeout = def.timeout_ms
		if timeout == nil then
			timeout = cfg.timeout_ms
		end
		local permissions = D.svc.permissions
		local scope_grants = permissions and permissions.grants and permissions:grants() or {}
		local ectx = {
			session_id = session.session_id,
			session_generation = session.session_epoch,
			lifetime = opts.lifetime,
			cancel = tok,
			run_cancel = run_cancel,
			tool = call.name,
			execution_id = execution_id,
			tool_call_id = call.id,
			approval_key = approval_key,
			approval_snapshot = approved_hash,
			projection_version = frozen.projection_version,
			projection_authorization = projection_authorization,
			cwd = mare.cwd,
			sandbox = sandbox,
			read_grants = effective_permissions and effective_permissions.read or scope_grants.read,
			unconfined = def.unconfined,
		}
		-- timeout policy: on timeout the call's token is cancelled first, so cooperative tools
		-- (bash: kill the job, return the output so far) stop on their own within the grace
		-- period; a tool that ignores its token is cut off after that
		local timed_out = false
		local deny_read_partial = false
		-- the host admits a call only once its `started` record is durable in the session
		-- journal: whatever on_state claimed, it never ran
		if opts.on_prepared then
			opts.on_prepared(mare.json.decode_lossless(mare.json.encode_strict(frozen)), table.copy(info))
		end
		local program_value
		local not_durable
		local which, ok, res = mare.task.select(function()
			if not approval_current() then
				return true, changed_refusal()
			end
			local function run_body()
				if tok:is_cancelled() or run_cancel:is_cancelled() then
					return true, { content = "Tool call cancelled", is_error = true, cancelled = true }
				end
				if effective_permissions then
					if permissions and permissions.commit_request and permissions:commit_request(frozen) == false then
						return true, changed_refusal()
					end
					if mare.subagent and mare.subagent.permission_commit then
						mare.subagent.permission_commit(frozen, effective_permissions, permission_scope)
					end
					ctx.bus:emit(
						"tool/authorized",
						{ call = frozen, permissions = effective_permissions, scope = permission_scope }
					)
				end
				local effective = mare.exec.current()
				local policy = effective and effective.sandbox
				deny_read_partial = type(policy) == "table" and #(policy.deny_read or {}) > 0
				if tok:is_cancelled() then
					return true, { content = "Tool call cancelled", is_error = true, cancelled = true }
				end
				local restored = args
				if projection_authorization then
					restored = mare.projection.restore(args, frozen.projection_version)
				elseif mare.projection.contains(args) then
					error("PROJECTION_CONTEXT: hidden arguments lack native human authorization")
				end
				local binding = opts.declarations and D.registry.resolve_binding(opts.declarations[call.name])
				local schema = binding and binding.schema or def._schema
				schema:validate(restored)
				local close_output = type(opts.on_output) == "function"
						and opts.audience ~= "program"
						and type(call.id) == "string"
						and call.id ~= ""
						and D.output_channel.begin_execution(call.id, {
							execution_id = execution_id,
							tool_call_id = call.id,
							session_id = session.session_id,
							session_epoch = session.session_epoch,
							on_output = opts.on_output,
						})
					or nil
				local _output_guard <close> = close_output and setmetatable({}, { __close = close_output }) or nil
				local success, result = protected_call(execute, restored, tctx)
				if not success then
					result = coded_failure(result) or { content = tostring(result), is_error = true }
				elseif type(result) == "string" then
					result = { content = result }
				end
				-- Keep raw bytes from the tool (main's behavior: postprocessing and the session
				-- writer handle them); a projection failure becomes a controlled result so the
				-- completion is still recorded.
				local projected_ok, projected =
					pcall(mare.projection.project, result or { content = "" }, { preserve_invalid = true })
				result = projected_ok and projected
					or {
						content = "Tool result could not be projected; completed effects were not rolled back: "
							.. tostring(projected),
						is_error = true,
						error_code = "postprocessing_failed",
					}
				if type(result) == "table" and ectx.denials then
					result.sandbox_denials = mare.json.decode_lossless(mare.json.encode_strict(ectx.denials))
				end
				local captured
				if opts.audience == "program" then
					local pok, value = pcall(D.results.program, result)
					captured = pok and value
						or { content = "Invalid tool program value: " .. tostring(value), is_error = true }
				end
				if opts.on_settled then
					opts.on_settled(mare.json.decode_lossless(mare.json.encode_strict(frozen)), table.copy(info))
				end
				local route = route_desc and table.copy(route_desc) or nil
				if route == nil and llm_svc and model then
					route = llm_svc.route_of and llm_svc:route_of(model) or model.route
				end
				local finished, display, canonical = pcall(D.results.finish, result, captured, def, frozen, opts, {
					max_bytes = retention_max_bytes,
					cancel = tok,
					run_cancel = run_cancel,
					execution_id = execution_id,
					session_id = session.session_id,
					session_generation = session.session_epoch,
					snapshot = frozen.snapshot,
					attempt = frozen.attempt,
					tool_call_id = frozen.id,
					cwd = frozen.cwd,
					route = route,
					route_snapshot = route_snapshot,
				})
				if not finished then
					display = {
						content = "Tool postprocessing failed; completed effects were not rolled back: "
							.. tostring(display),
						is_error = true,
						error_code = "postprocessing_failed",
					}
					canonical = display
				end
				program_value = canonical
				return true, display
			end
			-- Pure forwarding dispatchers (e.g. `call_tool`) delegate directly to the inner tool's
			-- top-level `mare.exec.run` so inner approved escalations and unconfined tools are not
			-- clamped by an outer un-elevated session baseline.
			local entered, a, b
			if D.dispatchers and D.dispatchers[call.name] then
				entered, a, b = pcall(run_body)
			else
				entered, a, b = pcall(mare.exec.run, ectx, run_body)
			end
			if entered then
				return a, b
			end
			not_durable = tostring(a):match("EXEC_NOT_DURABLE:[ \t]*([^\r\n]*)")
			if not not_durable then
				error(a, 0)
			end
		end, function()
			if def.timeout_policy == "managed" or timeout == false then
				tok:wait()
			elseif mare.task.sleep(timeout, tok) then
				timed_out = true
				tok:cancel()
			end
			mare.task.sleep(cfg.cancel_grace_ms)
		end)
		if running then
			running[tok] = nil
		end
		if not_durable then
			return {
				content = "Not run: the session can't be saved ("
					.. not_durable
					.. "); no tool runs until session writes work again.",
				is_error = true,
				error_code = "persist_failed",
				refused = true,
			},
				frozen
		end
		if which == 2 then
			if timed_out then
				return D.results.report_partial({
					content = ("Tool timed out after %d ms"):format(timeout),
					is_error = true,
					error_code = "tool_timeout",
				}, deny_read_partial)
			end
			return D.results.report_partial(
				{ content = "Tool call cancelled", is_error = true, cancelled = true },
				deny_read_partial
			)
		end
		if not ok then
			res = coded_failure(res) or { content = tostring(res), is_error = true }
		elseif type(res) == "string" then
			res = { content = res }
		end
		res = res or { content = "" }
		if timed_out then
			-- stopped by its own token after the timeout: still a timeout to the caller
			res.is_error = true
			res.error_code = "tool_timeout"
		end
		-- refusals recorded by the core policy layer: reliable evidence for targeted hints
		if ectx.denials then
			res.sandbox_denials = ectx.denials
		end
		local program_out = opts.audience == "program" and program_value ~= nil and clone(program_value) or nil
		local function project_result(value)
			if not mare.projection.active() then
				return value
			end
			local out, lossy = mare.projection.project(value, { lossy = true })
			if lossy and mare.ui and mare.ui.notify then
				mare.ui.notify("tool result contained non-UTF-8 bytes; replaced lossily with U+FFFD", "warning")
			end
			return out
		end
		return D.results.report_partial(project_result(res), deny_read_partial), frozen, program_out
	end

	-- One public service boundary covers refusals, errors and results alike. Partial chunks
	-- are withheld while values are registered: replacements on independent chunks leak splits.
	local execute_guarded = tools.execute
	function tools.execute(self, caller, call, opts)
		local safe = mare.projection.project(call)
		local options = table.copy(opts or {})
		local update = options.on_update
		options.on_update = function(partial)
			if update and not mare.projection.active() then
				update(partial)
			end
		end
		local ok, result, frozen, program_out = pcall(execute_guarded, self, caller, safe, options)
		if not ok then
			error(mare.projection.project(tostring(result)), 0)
		end
		local final_result = result
		if mare.projection.active() then
			local out, lossy = mare.projection.project(result, { lossy = true })
			if lossy and mare.ui and mare.ui.notify then
				mare.ui.notify("tool result contained non-UTF-8 bytes; replaced lossily with U+FFFD", "warning")
			end
			final_result = out
		end
		return final_result,
			mare.projection.project(frozen or safe),
			program_out ~= nil and mare.projection.project(program_out) or nil
	end

	return { coded_failure = coded_failure }
end
