-- Shared orchestration API for every tool implementation: declaration-bound nested calls.
-- The execution context owns this closure; callers never supply a forged parent identity.
return function(D)
	local ctx, tools = D.ctx, D.registry.tools
	local function attach(tctx, parent, parent_opts)
		local parent_record = parent_opts
				and parent_opts.declarations
				and D.registry.resolve_binding(parent_opts.declarations[parent.name])
			or nil
		local inherited_model = (parent_opts and parent_opts.model) or (parent_record and parent_record.model) or nil
		local inherited_step_id = (parent_opts and parent_opts.step_id ~= nil) and parent_opts.step_id
			or (parent_record and parent_record.step_id)
			or nil
		local alive, sequence, fault = true, 0, nil
		local parent_route_snapshot = parent_opts and parent_opts.route_snapshot or nil
		local session = D.svc.session
		---@type {session_id: string, session_epoch: string, ephemeral?: boolean}|nil
		local owner = session and session:info() or nil
		local ephemeral = (parent_opts and parent_opts.lifetime == "ephemeral-read-only")
			or (session and session:info().ephemeral)
			or (mare.args and mare.args.no_session)
		local function record(kind, data)
			if ephemeral then
				return true
			end
			if not session then
				return nil, "nested execution requires a session provider"
			end
			local current = session:info()
			if not owner or current.session_id ~= owner.session_id or current.session_epoch ~= owner.session_epoch then
				return nil, "session changed during nested execution"
			end
			local ok, err = pcall(session.add_record, session, kind, data, { persist = true })
			if not ok then
				return nil, type(err) == "table" and err.message or tostring(err)
			end
			return true
		end
		-- Bind once from real registry declarations. Invoke rechecks their definition versions.
		tctx.bind_tools = function(declarations, options)
			local bind_opts = {
				model = (options and options.model) or inherited_model,
				step_id = (options and options.step_id ~= nil) and options.step_id or inherited_step_id,
			}
			local bindings = tools:bind_declarations(ctx, declarations, bind_opts)
			options = options or {}
			local max_calls = options.max_calls == false and math.huge or (options.max_calls or 1024)
			local max_parallel = options.max_parallel or 10
			assert(
				(max_calls == math.huge or (math.type(max_calls) == "integer" and max_calls >= 0)),
				"invalid nested call budget"
			)
			assert(
				math.type(max_parallel) == "integer" and max_parallel >= 1 and max_parallel <= 128,
				"invalid nested parallel limit"
			)
			local attempts = 0
			local function perform(name, arguments, phase)
				phase:prepare()
				if not alive or tctx.cancel:is_cancelled() then
					return { content = "Nested execution cancelled", is_error = true, cancelled = true }
				end
				if fault then
					return { content = fault, is_error = true, error_code = "persist_failed" }
				end
				attempts = attempts + 1
				if attempts > max_calls then
					return { content = "Nested tool call budget exceeded", is_error = true, refused = true }
				end
				local binding_record = bindings[name] and D.registry.resolve_binding(bindings[name]) or nil
				local admit_model = (binding_record and binding_record.model) or inherited_model
				local admit_mode = binding_record and binding_record.edit_mode
				local admit_opts = {
					step_id = (binding_record and binding_record.step_id ~= nil) and binding_record.step_id
						or inherited_step_id,
					session_id = binding_record and binding_record.session_id,
					session_generation = binding_record and binding_record.session_generation,
				}
				if
					name == parent.name
					or not bindings[name]
					or (D.registry.registry[name] and D.registry.registry[name].allow_nested == false)
					or not D.registry.is_admitted(D.registry.registry[name], admit_model, admit_mode, admit_opts)
				then
					return {
						content = "Nested tool is not declared or invokes its parent",
						is_error = true,
						refused = true,
					}
				end
				sequence = sequence + 1
				local call = { id = parent.id .. "/nested/" .. sequence, name = name, arguments = arguments }
				local function persist_state(c, state, info)
					local ok, err = record(
						"tool/nested",
						{ parent_call_id = parent.id, tool_call_id = c.id, name = c.name, state = state }
					)
					if not ok then
						fault = tostring(err)
						return nil, fault
					end
					if ephemeral then
						return true
					end
					if not session.record_tool_state then
						fault = "session provider does not support record_tool_state"
						return nil, fault
					end
					local execution = mare.exec.current()
					local saved, err = pcall(session.record_tool_state, session, {
						tool_call_id = c.id,
						name = c.name,
						state = state,
						side_effect = info.side_effect,
						idempotent = info.idempotent,
						session_id = owner.session_id,
						session_epoch = owner.session_epoch,
						durable = true,
						cancel = info.cancel or (execution and execution.cancel),
					})
					if not saved then
						fault = tostring(type(err) == "table" and err.message or err)
						return nil, fault
					end
					return true
				end
				local token = tctx.cancel:child()
				local watcher
				if options and options.cancel then
					watcher = mare.task.spawn(function()
						options.cancel:wait()
						token:cancel()
					end)
				end
				local watcher_guard <close> = setmetatable({}, {
					__close = function()
						if watcher then
							watcher:abort()
						end
					end,
				})
				local audience = options.audience or "program"
				local forwarded_sandbox = parent_opts and parent_opts.sandbox or nil
				if options.sandbox ~= nil then
					forwarded_sandbox = forwarded_sandbox ~= nil
							and (mare.proc.sandbox.intersect(forwarded_sandbox, options.sandbox) or forwarded_sandbox)
						or options.sandbox
				end
				local forwarded_on_output = nil
				if audience == "model" then
					if type(options.on_output) == "function" then
						forwarded_on_output = options.on_output
					elseif type(parent_opts and parent_opts.on_output) == "function" then
						local parent_on_output = parent_opts.on_output
						local parent_exec_id = parent_opts.execution_id
						forwarded_on_output = function(blocks, meta)
							local mapped = type(meta) == "table" and table.copy(meta) or {}
							mapped.inner_tool_call_id = mapped.tool_call_id
							mapped.inner_execution_id = mapped.execution_id
							mapped.tool_call_id = parent.id
							if parent_exec_id ~= nil then
								mapped.execution_id = parent_exec_id
							end
							return parent_on_output(blocks, mapped)
						end
					end
				end
				local res, frozen, program = tools:execute(ctx, call, {
					lifetime = parent_opts and parent_opts.lifetime or nil,
					cancel = token,
					run_cancel = options.cancel or tctx.run_cancel or tctx.cancel,
					audience = audience,
					declarations = bindings,
					route_snapshot = parent_route_snapshot,
					model = admit_model,
					step_id = admit_opts.step_id,
					sandbox = forwarded_sandbox,
					on_update = options.on_update,
					on_output = forwarded_on_output,
					origin = options.origin,
					approve = options.approve or (parent_opts and parent_opts.approve),
					on_state = persist_state,
					on_settled = function()
						phase:commit()
					end,
					on_prepared = function(frozen)
						phase:body(frozen)
					end,
				})
				phase:body(frozen or call)
				if not phase.committing then
					phase:commit()
				end
				local ok, err = record("tool/nested-result", {
					parent_call_id = parent.id,
					tool_call_id = call.id,
					name = name,
					-- Keep result outside the conversation tree. Never replay an incomplete call.
					result = {
						content = res.content,
						details = res.details,
						is_error = res.is_error,
						cancelled = res.cancelled,
					},
				})
				if ok and not res.refused then
					local side_effect, idempotent = tools:side_effect(ctx, name)
					ok, err = persist_state(
						frozen or call,
						"completed",
						{ side_effect = side_effect, idempotent = idempotent }
					)
				end
				if not ok then
					fault = tostring(err)
					return { content = fault, is_error = true, error_code = "persist_failed" }
				end
				if audience == "model" then
					return res
				end
				return program or res
			end
			local ordered = require("tools.ordered")(
				function(call)
					return tools:concurrency_safe(ctx, call)
				end,
				max_parallel,
				function()
					fault = "Nested execution interrupted before commit"
				end
			)
			local function run(name, arguments, phase)
				local guard <close> = setmetatable({}, {
					__close = function()
						phase:abort()
					end,
				})
				local ok, result = pcall(perform, name, arguments, phase)
				if not phase.committing then
					phase:body()
					phase:commit()
				end
				phase:finish()
				if not ok then
					return { content = tostring(result), is_error = true }
				end
				return result
			end
			local function invoke(name, arguments)
				return run(name, arguments, ordered.reserve({ name = name, arguments = arguments }))
			end
			local function batch(calls)
				assert(type(calls) == "table" and #calls <= 128, "invalid nested batch")
				local tasks, results = {}, mare.json.array()
				for index, call in ipairs(calls) do
					assert(type(call) == "table" and type(call.name) == "string", "invalid nested batch call")
					local phase = ordered.reserve({ name = call.name, arguments = call.arguments })
					tasks[index] = mare.task.spawn(function()
						return run(call.name, call.arguments, phase)
					end)
				end
				for index, task in ipairs(tasks) do
					results[index] = task:wait()
				end
				return results
			end
			return invoke, batch
		end
		return function()
			alive = false
		end
	end
	return { attach = attach }
end
