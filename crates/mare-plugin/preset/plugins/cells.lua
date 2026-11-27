-- General controlled script service shared by Lua orchestrators and RPC clients.
return {
	name = "cells",
	inject = { "tools", "llm", "task" },
	provide = { "cells" },
	config_schema = {
		timeout_ms = { type = "integer", default = 60000, desc = "Script timeout in milliseconds." },
		memory_bytes = { type = "integer", default = 33554432, desc = "Memory limit of a script VM." },
		max_bytes = {
			type = "integer",
			default = 1048576,
			desc = "Byte cap of sandbox data; also the default of the message and output caps.",
		},
		max_message_bytes = { type = "integer", default = 0, desc = "Largest message; 0 inherits max_bytes." },
		max_output_bytes = { type = "integer", default = 0, desc = "Largest output; 0 inherits max_bytes." },
		max_pending_calls = { type = "integer", default = 128, desc = "Pending host calls allowed at once." },
		stop_grace_ms = { type = "integer", default = 3000, desc = "Grace period when stopping a script." },
		grace_ms = { type = "integer", default = 3000, desc = "Grace period of a script run in milliseconds." },
		max_instructions = { type = "integer", default = 10000000, desc = "Instruction budget of a script." },
		max_sub_calls_per_minute = {
			type = "integer",
			default = 1024,
			desc = "Rate cap of tool sub-calls from a script.",
		},
		max_parallel_sub_calls = { type = "integer", default = 10, desc = "Parallel tool sub-calls from a script." },
		execution_mode = {
			type = "enum",
			choices = { "per-call", "persistent-in-memory", "vm-script" },
			default = "per-call",
			desc = "How script VMs are reused between calls.",
		},
		script_persistence = { type = "boolean", default = false, desc = "Persist scripts across sessions." },
	},
	apply = function(ctx, cfg)
		local lib = require("mare_lib")
		-- the optional session and mcp services, read on each call
		local svc = lib.followers(ctx, { "session", "mcp" })
		local active = {}
		local executions = {}
		local execution_options = {}
		local owners = {}
		local cells
		local function session_info()
			return svc.session and svc.session:info() or { ephemeral = true }
		end
		local pool = require("cells.pool")(ctx, cfg, svc)
		ctx:on("session/changed", function()
			for _, token in pairs(active) do
				token:cancel()
			end
			for _, token in pairs(executions) do
				token:cancel()
			end
			for _, owner in pairs(owners) do
				owner.memory.values = {}
				owner.memory.generation = session_info().session_epoch
			end
		end)
		ctx:effect(function()
			return function()
				for _, token in pairs(active) do
					token:cancel()
				end
			end
		end, "cleanup")
		local function catalog()
			local defs = ctx.tools:definitions(ctx.llm:current())
			if svc.mcp then
				local names = {}
				for _, d in ipairs(svc.mcp:catalog("codemode")) do
					names[#names + 1] = d.name
				end
				if #names > 0 then
					for _, d in ipairs(ctx.tools:definitions(ctx.llm:current(), names)) do
						defs[#defs + 1] = d
					end
				end
			end
			local result, seen = mare.json.array(), {}
			local describe = require("cells.sdk")()
			for _, d in ipairs(defs) do
				if ctx.tools:get(d.name).allow_nested ~= false and not seen[d.name] then
					seen[d.name] = true
					result[#result + 1] = describe(d, ctx.tools:get(d.name))
				end
			end
			return result
		end
		local function execute(code, options, tctx)
			local runtime_options = execution_options[tctx.tool_call_id] or options or {}
			local owner = runtime_options.owner
			if not owner then
				return { content = "A live execution owner is required", is_error = true }
			end
			local memory = owner.memory
			local token = tctx.cancel:child()
			local script_context = table.copy(tctx)
			script_context.cancel = token
			executions[tctx.tool_call_id] = token
			owner.direct[tctx.tool_call_id] = token
			owner.direct_keys[tctx.tool_call_id] = runtime_options.state_owner
			local execution_guard <close> = setmetatable({}, {
				__close = function()
					token:cancel()
					executions[tctx.tool_call_id] = nil
					owner.direct[tctx.tool_call_id] = nil
					owner.direct_keys[tctx.tool_call_id] = nil
				end,
			})
			local args = table.copy(options or {})
			args.code = code
			if svc.mcp and type(args.capabilities) == "table" then
				for _, name in ipairs(args.capabilities) do
					if type(name) == "string" and not ctx.tools:get(name) then
						svc.mcp:catalog("codemode")
						break
					end
				end
			end
			args.on_output = runtime_options.on_output
			args.on_notify = runtime_options.on_notify
			args.on_yield = runtime_options.on_yield
			if not args.on_notify then
				local channel = runtime_options.output_channel or options.output_channel
				if not channel and lib.callable(ctx.tools.output_channel) then
					channel = ctx.tools:output_channel()
				end
				args.on_notify = function(block)
					if not channel then
						error("notify has no active output channel", 0)
					end
					local receipt = ctx.tools:send_output(channel, mare.json.array({ block }), { cancel = token })
					if receipt.status ~= "accepted" then
						error("notify rejected: " .. tostring(receipt.error), 0)
					end
				end
			end
			local generation = session_info().session_epoch
			if memory.generation ~= generation then
				memory.values = {}
				memory.generation = generation
			end
			return require("cells.execute")(ctx, cfg, args, script_context, memory, pool)
		end
		ctx.tools:register("_sandbox_script", {
			description = "Run an explicitly capability-bound Lua script",
			allow_nested = false,
			exposure = "nested",
			side_effect = "external",
			idempotent = false,
			timeout_ms = false,
			parameters = {
				type = "object",
				additionalProperties = false,
				required = { "code", "capabilities" },
				properties = {
					code = { type = "string" },
					capabilities = { type = "array", items = { type = "string" } },
					timeout_ms = { oneOf = { { type = "integer" }, { type = "boolean", enum = { false } } } },
					state_owner = { type = "string" },
				},
			},
			execute = function(args, tctx)
				return execute(args.code, args, tctx)
			end,
		})
		local function ensure_not_resetting(owner)
			if owner.resetting then
				lib.fail("busy", "execution owner is resetting")
			end
		end
		local function owned(caller)
			local id = lib.caller_id(caller) or lib.caller_id(ctx)
			local owner = owners[id]
			if not owner then
				owner = {
					runs = {},
					run_keys = {},
					direct = {},
					direct_keys = {},
					keys = {},
					memory = { values = {}, generation = session_info().session_epoch },
				}
				-- the owner's state ends with the caller's fiber
				lib.owned(caller or ctx, function()
					for _, token in pairs(owner.runs) do
						token:cancel()
					end
					for key in pairs(owner.keys) do
						pool.discard(key)
					end
					for _, token in pairs(owner.direct) do
						token:cancel()
					end
					if cells then
						cells.clear(id)
					end
					owners[id] = nil
				end)
				owners[id] = owner
			end
			return id, owner
		end
		local function run(_, caller, code, options)
			options = table.copy(options or {})
			local caller_id, owner = owned(caller)
			ensure_not_resetting(owner)
			options.owner = owner
			options.state_owner = tostring(caller_id) .. "/" .. (options.state_owner or "session")
			owner.keys[options.state_owner] = true
			local cell_id = options.cell_id or mare.crypto.uuid()
			if type(cell_id) ~= "string" or #cell_id == 0 or #cell_id > 128 then
				lib.fail("invalid_arguments", "invalid cell_id")
			end
			local key = tostring(caller_id or "host") .. "/" .. cell_id
			if active[key] then
				lib.fail("cells.duplicate_cell", "cell_id is already active: " .. cell_id)
			end
			local token = options.cancel and options.cancel:child() or mare.task.token()
			active[key] = token
			owner.runs[cell_id] = token
			owner.run_keys[cell_id] = options.state_owner
			execution_options["sandbox/" .. key] = options
			local guard <close> = setmetatable({}, {
				__close = function()
					token:cancel()
					active[key] = nil
					owner.runs[cell_id] = nil
					owner.run_keys[cell_id] = nil
					execution_options["sandbox/" .. key] = nil
				end,
			})
			local sess_owner = session_info()
			local call = {
				id = "sandbox/" .. key,
				name = "_sandbox_script",
				arguments = {
					code = code,
					capabilities = options.capabilities
							and (#options.capabilities > 0 and options.capabilities or mare.json.array())
						or mare.json.array(),
					timeout_ms = options.timeout_ms,
					state_owner = options.state_owner,
				},
			}
			local function state(c, phase, info)
				if not svc.session or session_info().ephemeral then
					return true
				end
				if not svc.session.record_tool_state then
					return nil, "session provider does not support record_tool_state"
				end
				-- Explicit cell lifetime, with the same facade/ACK and frozen owner.
				local saved, err = pcall(svc.session.record_tool_state, svc.session, {
					tool_call_id = c.id,
					name = c.name,
					state = phase,
					side_effect = info.side_effect,
					idempotent = info.idempotent,
					session_id = sess_owner.session_id,
					session_epoch = sess_owner.session_epoch,
					durable = true,
					cancel = token,
				})
				if not saved then
					return nil, type(err) == "table" and err.message or tostring(err)
				end
				return true
			end
			local declarations =
				ctx.tools:bind_declarations(ctx.tools:definitions(ctx.llm:current(), { "_sandbox_script" }))
			local display, _, canonical = ctx.tools:execute(
				call,
				{ audience = "program", cancel = token, declarations = declarations, on_state = state }
			)
			local result = canonical or display
			if svc.session and not session_info().ephemeral then
				local ok, err = pcall(
					svc.session.add_record,
					svc.session,
					"sandbox/run-result",
					{ id = key, result = result.details, error = result.is_error and result.content or nil },
					{ persist = true }
				)
				if ok and not result.refused then
					ok, err = state(call, "completed", { side_effect = "external", idempotent = false })
				end
				if not ok then
					return {
						is_error = true,
						error = "sandbox result persistence failed: "
							.. tostring(type(err) == "table" and err.message or err),
						output = "",
						cancelled = token:is_cancelled(),
					}
				end
			end
			local detail = result.details
				or {
					output = "",
					error = type(result.content) == "string" and result.content
						or mare.json.encode_strict(result.content),
					is_error = result.is_error == true,
					cancelled = result.cancelled == true,
				}
			detail.cell_id = cell_id
			return detail
		end
		cells = require("cells.cells")(ctx, run)
		local function reset(caller)
			local id, owner = owned(caller)
			ensure_not_resetting(owner)
			owner.resetting = true
			local guard <close> = setmetatable({}, {
				__close = function()
					owner.resetting = false
				end,
			})
			cells.clear(id)
			for _, token in pairs(owner.runs) do
				token:cancel()
			end
			for _, token in pairs(owner.direct) do
				token:cancel()
			end
			local deadline = mare.task.clock() + 5000
			while next(owner.runs) or next(owner.direct) do
				if mare.task.clock() >= deadline then
					lib.fail("cells.stop_unconfirmed", "active cell cleanup did not confirm before reset deadline")
				end
				mare.task.sleep(10)
			end
			for key in pairs(owner.keys) do
				if not pool.release(key) then
					lib.fail("cells.stop_unconfirmed", "retained worker cleanup did not confirm during reset")
				end
			end
			owner.keys = {}
			owner.memory.values = {}
			owner.memory.generation = session_info().session_epoch
			return {}
		end
		lib.service(ctx, "cells", {
			start = function(_, caller, code, options)
				local caller_id, owner = owned(caller)
				ensure_not_resetting(owner)
				if type(options) == "table" then
					if
						options.timeout_ms ~= nil
						and options.timeout_ms ~= false
						and (
							math.type(options.timeout_ms) ~= "integer"
							or options.timeout_ms < 1
							or options.timeout_ms > 600000
						)
					then
						lib.fail("invalid_arguments", "timeout_ms must be between 1 and 600000, or false")
					end
					if
						options.state_owner ~= nil
						and (
							type(options.state_owner) ~= "string"
							or #options.state_owner == 0
							or #(tostring(caller_id) .. "/" .. options.state_owner) > 256
						)
					then
						lib.fail("invalid_arguments", "state_owner must be non-empty and within 256 bytes with owner")
					end
				end
				return cells.start(code, options, caller)
			end,
			wait = function(_, caller, cell_id, options)
				return cells.wait(cell_id, options, lib.caller_id(caller) or lib.caller_id(ctx))
			end,
			list = function(_, caller, opts)
				return lib.paginate(cells.list(lib.caller_id(caller) or lib.caller_id(ctx)), opts)
			end,
			reset = function(_, caller)
				return reset(caller)
			end,
			get = function(_, caller)
				local _, owner = owned(caller)
				local count = 0
				for _ in pairs(owner.direct) do
					count = count + 1
				end
				local keys = 0
				for _ in pairs(owner.memory.values) do
					keys = keys + 1
				end
				return { running = count, stored_keys = keys, retains_state = pool.retained, store_durable = false }
			end,
			list_tools = function(_, _, opts)
				return lib.paginate(catalog(), opts)
			end,
			cancel = function(_, caller, cell_id)
				local caller_id = lib.caller_id(caller) or lib.caller_id(ctx)
				local was_running = cells.cancel(caller_id, cell_id)
				if was_running == nil then
					lib.fail("cells.unknown_cell", "unknown cell: " .. tostring(cell_id))
				end
				return {}
			end,
		})
		ctx.tools:register_executor("lua", {
			api_version = 1,
			exposures = { "codemode" },
			run = function(spec, caller)
				return run(nil, caller, spec.code, spec)
			end,
		})
	end,
}
