-- tools.registry: the tool definition stacks, the `tools` service table and its lookup /
-- declaration methods (register, get, list, snippets, definitions, edit mode);
-- part of the `tools` plugin (plugins/tools.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local tool_allowed = mare.subagent and mare.subagent.tool_allowed

	-- name -> stack of definitions: a later registration shadows an earlier one and, when
	-- its owner is disposed, the previous definition comes back (Cordis-style).
	local stacks = {}
	--- the model can add deferred tools mid-conversation (Anthropic / OpenAI Responses / Kimi)
	local function native_tool_update(model)
		return model ~= nil and model.tool_update == "addition_only"
	end
	local registry = setmetatable({}, {
		__index = function(_, name)
			local st = stacks[name]
			return st and st[#st] or nil
		end,
	})
	-- tool names in the order they are listed (prompt, request declarations): by the
	-- registering plugin's row, then by registration within it, so the list (and the
	-- provider's cached prompt prefix) does not depend on when each plugin's fiber activated
	local def_seq = 0
	local order = {}
	local placed = {} -- name -> { row, seq }
	local function row_of(caller)
		-- an old-style ctx's name or a cordis ctx's fiber name (a mare.Service method's caller is the
		-- traced cordis ctx, which has no `name`)
		local name = require("mare_lib").caller_name(caller)
		-- kernel:plugin_row, not kernel:plugins: allowed while registering for a peer
		local row = name and ctx.kernel:plugin_row(name) or 0
		return row > 0 and row or math.huge -- not a row (a test or host ctx): after the rows, in arrival order
	end
	local function place(caller, name)
		local key = { row_of(caller), def_seq }
		placed[name] = key
		local at = #order + 1
		for i, other in ipairs(order) do
			local k = placed[other]
			if k[1] > key[1] or (k[1] == key[1] and k[2] > key[2]) then
				at = i
				break
			end
		end
		table.insert(order, at, name)
	end

	local tools = {}
	local declarations = setmetatable({}, { __mode = "k" })
	local bindings = setmetatable({}, { __mode = "k" })
	-- bumped whenever a tool is added or removed: the agent re-reads the declarations between
	-- requests only when it changed
	local generation = 0
	--- side effects: side_effect = "read" | "write" (local files only) |
	--- "external" (anything beyond this machine's files: shell, network, git push…), plus
	--- `idempotent`. Undeclared tools are treated as external and non-idempotent; a
	--- `readonly = true` tool is "read". `unconfined = true` marks tools executed outside this
	--- process (MCP servers, RPC hosts): the local sandbox can't confine them.
	local function normalize(def)
		assert(def.timeout_policy == nil or def.timeout_policy == "managed", "invalid tool timeout_policy")
		assert(def.access == nil or type(def.access) == "function", "tool access must be a function")
		if def.side_effect == nil then
			def.side_effect = def.readonly == true and "read" or "external"
		end
		if def.readonly == nil then
			def.readonly = def.side_effect == "read"
		end
		if def.idempotent == nil then
			def.idempotent = def.side_effect == "read"
		end
		def.unconfined = def.unconfined == true
		if def.sensitive_inputs ~= nil then
			assert(type(def.sensitive_inputs) == "table", "sensitive_inputs must be a string list")
			local fields, seen, count = {}, {}, 0
			for index, field in pairs(def.sensitive_inputs) do
				assert(
					type(index) == "number"
						and index % 1 == 0
						and index > 0
						and type(field) == "string"
						and field ~= ""
						and not seen[field],
					"sensitive_inputs must be a unique dense string list"
				)
				fields[index], seen[field], count = field, true, count + 1
			end
			assert(count == #fields, "sensitive_inputs must be a dense string list")
			def.sensitive_inputs = fields
		end
		if def.command ~= nil then
			assert(type(def.command) == "table", "command declaration must be a table")
			if def.command.shell ~= nil or def.command.arg ~= nil then
				assert(
					def.command.argv == nil and def.command.defaults == nil,
					"command declaration cannot mix {shell, arg} and {argv}"
				)
				assert(
					type(def.command.shell) == "string"
						and def.command.shell ~= ""
						and type(def.command.arg) == "string"
						and def.command.arg ~= "",
					"command shell declaration requires non-empty shell and arg strings"
				)
				def.command = { shell = def.command.shell, arg = def.command.arg }
			elseif def.command.argv ~= nil then
				assert(
					type(def.command.argv) == "table" and #def.command.argv > 0,
					"command argv declaration must be a non-empty string list"
				)
				local argv, count = {}, 0
				for index, item in pairs(def.command.argv) do
					assert(
						type(index) == "number" and index % 1 == 0 and index > 0 and type(item) == "string",
						"command.argv must be a dense string list"
					)
					argv[index], count = item, count + 1
				end
				assert(count == #argv, "command.argv must be a dense string list")
				assert(
					argv[1] ~= "" and not argv[1]:find("{", 1, true),
					"command template executable must be a literal string"
				)
				local defaults
				if def.command.defaults ~= nil then
					assert(type(def.command.defaults) == "table", "command.defaults must be a table")
					defaults = {}
					for k, v in pairs(def.command.defaults) do
						assert(type(k) == "string", "command.defaults keys must be strings")
						local vt = type(v)
						assert(
							vt == "string" or vt == "number" or vt == "boolean",
							"command.defaults values must be primitive"
						)
						defaults[k] = v
					end
					if next(defaults) == nil then
						defaults = nil
					end
				end
				def.command = { argv = argv, defaults = defaults }
			else
				error("command declaration requires either {shell, arg} or {argv}")
			end
		end
	end

	local function compile_schema(name, params, param_order)
		-- Validate the original value before adding the ordering hint. Copying or overwriting
		-- first could hide an unsupported metatable or an invalid x-mare-order annotation.
		local ok, schema = pcall(mare.schema.compile, params)
		if not ok then
			error(("tool %s schema: %s"):format(tostring(name), tostring(schema)))
		end
		if param_order ~= nil then
			local value = schema:value()
			value["x-mare-order"] = param_order
			ok, schema = pcall(mare.schema.compile, value)
			if not ok then
				error(("tool %s schema: %s"):format(tostring(name), tostring(schema)))
			end
		end
		return schema
	end

	local owned = require("mare_lib").owned

	function tools.register(_, caller, name, def, parent)
		local is_rpc = require("mare_lib").rpc_caller_id(caller) ~= nil
		-- Compile before changing the stack or generation; isolate the caller's table.
		def = table.copy(def)
		def.name = name
		if is_rpc then
			local fail = require("mare_lib").fail
			if type(def.description) ~= "string" or def.description == "" then
				fail("invalid_arguments", "tool description must be a non-empty string")
			end
			if type(def.parameters) ~= "table" then
				fail("invalid_arguments", "tool parameters must be a schema object")
			end
			if type(def.execute) ~= "function" then
				fail("invalid_arguments", "tool execute callback is required")
			end
			if def.access ~= nil then
				if type(def.access) ~= "table" then
					fail("invalid_arguments", "tool access must be an array of declarations")
				end
				local access_specs = def.access --[[@as table[] ]]
				local decls = {}
				local props = type(def.parameters.properties) == "table" and def.parameters.properties or nil
				local req_set = {}
				if type(def.parameters.required) == "table" then
					for _, req_name in ipairs(def.parameters.required) do
						if type(req_name) == "string" then
							req_set[req_name] = true
						end
					end
				end
				for i, item in ipairs(access_specs) do
					if
						type(item) ~= "table"
						or item.kind ~= "file"
						or (item.op ~= "read" and item.op ~= "write" and item.op ~= "create" and item.op ~= "delete")
						or item.recursive ~= false
						or type(item.path_arg) ~= "string"
						or item.path_arg == ""
						or not req_set[item.path_arg]
						or type(props) ~= "table"
						or type(props[item.path_arg]) ~= "table"
						or props[item.path_arg].type ~= "string"
					then
						fail("invalid_arguments", "tool access requires a non-recursive required string path_arg")
					end
					decls[i] = { kind = item.kind, op = item.op, path_arg = item.path_arg, recursive = false }
				end
				def.access = function(args, _ctx)
					local result = {}
					for i, item in ipairs(decls) do
						local path = args and args[item.path_arg]
						assert(type(path) == "string", "missing declared access path argument: " .. item.path_arg)
						result[i] = { kind = item.kind, op = item.op, path = path, recursive = false }
					end
					return result
				end
			end
			def.unconfined = true
			---@type fun(args: table, ctx?: table, update?: function, cancel?: any): any
			local raw_execute = def.execute
			def.execute = function(args, tctx)
				local cb_ctx = {
					tool_call_id = tctx and tctx.tool_call_id or nil,
					idempotency_key = tctx and tctx.idempotency_key or nil,
					cwd = tctx and tctx.cwd or nil,
				}
				return raw_execute(args, cb_ctx, tctx and tctx.update or nil, tctx and tctx.cancel or nil)
			end
		end
		if def.admit ~= nil then
			assert(type(def.admit) == "function", "tool admit must be a function")
		end
		if def.param_order ~= nil then
			-- Validate and detach the original hint even for dynamic parameter functions.
			local hint = compile_schema(name, { type = "object" }, def.param_order):value()
			def.param_order = hint["x-mare-order"]
		end
		if type(def.freeform) == "table" then
			def.freeform = table.copy(def.freeform)
		end
		def._removed, def._schema, def._parent, def._remove = nil, nil, parent, nil
		if type(def.parameters) ~= "function" then
			local ok, schema_or_err = pcall(compile_schema, name, def.parameters, def.param_order)
			if not ok then
				if is_rpc then
					require("mare_lib").fail("invalid_arguments", tostring(schema_or_err))
				end
				error(schema_or_err, 0)
			end
			def._schema = schema_or_err
			def.parameters = schema_or_err:value()
		end
		if def.output_schema ~= nil then
			def._output_validator = mare.schema.compile_value(def.output_schema)
			def.output_schema = def._output_validator:value()
		end
		normalize(def)
		local st = stacks[name]
		if not st then
			st = {}
			stacks[name] = st
			place(caller, name)
		end
		table.insert(st, def)
		def_seq = def_seq + 1
		generation = generation + 1
		def._version = def_seq -- the snapshot binds approvals to this exact definition
		def._running = {} -- cancel tokens of calls in flight (cancelled when the owner goes away)
		-- remove this definition (hot reload / unload, or the owner withdrawing one tool, e.g.
		-- an MCP server's tools/list_changed): calls of it still running are cancelled
		local function remove()
			if def._removed then
				return
			end
			def._removed = true
			-- A derived definition cannot outlive the definition it wraps. This also
			-- revokes descendants created by other plugins, before exposing a fallback.
			local descendants = {}
			for _, candidate in ipairs(stacks[name] or {}) do
				if candidate._parent == def then
					descendants[#descendants + 1] = candidate
				end
			end
			for _, child in ipairs(descendants) do
				child._remove()
			end
			for tok in pairs(def._running or {}) do
				tok:cancel()
			end
			local cur = stacks[name]
			local i = cur and table.find(cur, def)
			if i then
				table.remove(cur, i)
			end
			if cur and #cur == 0 then
				stacks[name] = nil
				local j = table.find(order, name)
				if j then
					table.remove(order, j)
				end
				placed[name] = nil
			end
			generation = generation + 1
		end
		remove = owned(caller, remove, "tool " .. name)
		def._remove = remove
		return remove, function()
			return not def._removed
		end
	end

	-- Wrap only the current exact version; its removal revokes this shadow too.
	function tools.shadow(self, caller, name, def)
		local parent = registry[name]
		if not parent or def._version ~= parent._version then
			error("cannot shadow changed or missing tool " .. tostring(name))
		end
		return self.register(self, caller, name, def, parent)
	end

	--- revision() -> number that changes whenever a tool is added or removed
	function tools.invalidate(_, _)
		generation = generation + 1
	end
	function tools.revision(_, _)
		return generation
	end
	function tools.has_dynamic_admission(_, _)
		for _, n in ipairs(order) do
			local d = registry[n]
			if d and d.admit ~= nil then
				return true
			end
		end
		return false
	end

	local function public_definition(def)
		if not def then
			return nil
		end
		local copy = table.copy(def)
		copy._running, copy._schema, copy._removed, copy._parent, copy._remove = nil, nil, nil, nil, nil
		copy._output_validator = nil
		if def._schema then
			copy.parameters = def._schema:value()
		end
		if type(def.param_order) == "table" then
			copy.param_order = table.copy(def.param_order)
		end
		if def.sensitive_inputs then
			copy.sensitive_inputs = table.copy(def.sensitive_inputs)
		end
		if type(def.freeform) == "table" then
			copy.freeform = table.copy(def.freeform)
		end
		if type(def.command) == "table" then
			copy.command = require("mare_lib").copy_deep(def.command)
		end
		return copy
	end
	function tools.get(_, _, name)
		if tool_allowed and not tool_allowed(name) then
			return nil
		end
		return public_definition(registry[name])
	end
	--- readonly(name) -> true when the tool declares it has no side effects (may run in parallel)
	function tools.readonly(_, _, name)
		local d = registry[name]
		return d ~= nil and d.readonly == true
	end
	--- concurrency_safe(call) -> bool: may this call overlap other safe calls? A tool may decide per call from its arguments with
	--- `concurrency_safe = function(args) -> bool` (or a boolean); the default is `readonly`.
	--- An error in the predicate counts as unsafe (the call runs alone).
	function tools.concurrency_safe(_, _, call)
		local d = registry[call.name]
		if not d then
			return false
		end
		local f = d.concurrency_safe
		if type(f) == "function" then
			local args = call.arguments
			if type(args) ~= "table" then
				return false
			end
			local ok, v = pcall(f, args)
			return ok and v == true
		elseif type(f) == "boolean" then
			return f
		end
		return d.readonly == true
	end
	--- side_effect(name) -> side_effect, idempotent, unconfined
	function tools.side_effect(_, _, name)
		local d = registry[name]
		if not d then
			return "external", false, false
		end
		return d.side_effect, d.idempotent == true, d.unconfined == true
	end
	function tools.list(_, _)
		local out = {}
		for _, n in ipairs(order) do
			if not tool_allowed or tool_allowed(n) then
				out[#out + 1] = public_definition(registry[n])
			end
		end
		return out
	end

	local function edit_mode(model)
		local mode = cfg.edit_mode
		if mode == "auto" then
			if model and model.edit_tool then
				return model.edit_tool
			end
			local llm = D.svc.llm
			if model and llm then
				local resolved = (model.ref and llm:resolve(model.ref))
					or (model.provider and model.id and llm:resolve(model.provider .. "/" .. model.id))
				if resolved and resolved.edit_tool then
					return resolved.edit_tool
				end
				local def_edit = llm:model_default(model, "edit_tool")
				if def_edit then
					return def_edit
				end
			end
			return "replace"
		end
		return mode
	end
	function tools.edit_mode(_, _, model)
		return edit_mode(model)
	end

	local function admission_context(model, mode, opts)
		local session_svc = D.svc.session
		---@type {session_id: string, session_epoch: string}|nil
		local session_info = session_svc and session_svc:info() or nil
		return {
			model = model,
			edit_mode = mode,
			step_id = opts and opts.step_id or nil,
			session = session_info,
		}
	end

	local function is_admitted(def, model, mode, opts, admit_ctx)
		if not def then
			return false
		end
		local context = admit_ctx or admission_context(model, mode, opts)
		if opts and opts.session_id ~= nil then
			if not context.session or context.session.session_id ~= opts.session_id then
				return false
			end
		end
		if opts and opts.session_generation ~= nil then
			if not context.session or context.session.session_epoch ~= opts.session_generation then
				return false
			end
		end
		if def.admit == nil then
			return true
		end
		local ok, admitted = pcall(def.admit, context)
		return ok and admitted == true
	end

	--- the user turned the tool off (`mare.tools.disable`, `tools_disabled`, `--exclude-tools`) or `--tools a,b,c` leaves it out
	local function excluded(n)
		if tools.is_disabled and tools.is_disabled(tools, ctx, n) then
			return true
		end
		local only = mare.config.tools_enabled
		return type(only) == "table" and not table.find(only, n)
	end

	--- one-line snippets for the system prompt
	function tools.snippets(_, _, model, opts)
		local mode = edit_mode(model)
		local admit_ctx = admission_context(model, mode, opts)
		local out = {}
		for _, n in ipairs(order) do
			local d = registry[n]
			if
				d.snippet
				and d.exposure ~= "deferred"
				and d.exposure ~= "nested"
				and not excluded(n)
				and (not tool_allowed or tool_allowed(n))
				and (not d.enabled or d.enabled(model, mode))
				and is_admitted(d, model, mode, opts, admit_ctx)
			then
				local guidelines = d.guidelines
				if type(guidelines) == "function" then
					guidelines = guidelines(model, mode)
				end
				out[#out + 1] = { name = n, snippet = d.snippet, guidelines = guidelines }
			end
		end
		return out
	end

	--- Sorted names of deferred tools advertised in `<tools>` for `tool_search`.
	--- Always keeps the full sorted list across loads so the system prompt prefix never mutates
	--- when tools are discovered.
	local function deferred_names(model, opts)
		local mode = edit_mode(model)
		local admit_ctx = admission_context(model, mode, opts)
		local out = {}
		for _, n in ipairs(order) do
			local d = registry[n]
			if
				d.exposure == "deferred"
				and not excluded(n)
				and (not tool_allowed or tool_allowed(n))
				and (not d.enabled or d.enabled(model, mode))
				and is_admitted(d, model, mode, opts, admit_ctx)
			then
				out[#out + 1] = n
			end
		end
		table.sort(out)
		return out
	end

	--- definitions(model, names?, opts?) -> request tool list
	function tools.definitions(_, _, model, names, opts)
		local mode = edit_mode(model)
		local admit_ctx = admission_context(model, mode, opts)
		local out = {}
		for _, n in ipairs(order) do
			local d = registry[n]
			local ok = not excluded(n) and (not tool_allowed or tool_allowed(n))
			if d.exposure == "nested" and not names then
				ok = false
			elseif not names and d.exposure == "deferred" and not native_tool_update(model) then
				ok = false
			end
			if names and not table.find(names, n) then
				ok = false
			end
			if ok and d.enabled then
				ok = d.enabled(model, mode)
			end
			if ok then
				ok = is_admitted(d, model, mode, opts, admit_ctx)
			end
			if ok then
				local desc = d.description
				if type(desc) == "function" then
					desc = desc(model, mode)
				end
				local schema = d._schema
				if not schema then
					local ok, params = pcall(d.parameters, model, mode)
					if not ok then
						error(("tool %s schema: %s"):format(n, tostring(params)))
					end
					schema = compile_schema(n, params, d.param_order)
				end
				local defer_loading = d.exposure == "deferred" and native_tool_update(model)
				local declaration = {
					name = n,
					description = desc,
					parameters = schema:value(),
					defer_loading = defer_loading and true or nil,
				}
				declarations[declaration] = {
					schema = schema,
					definition = d,
					version = d._version,
					model = model,
					edit_mode = mode,
					step_id = opts and opts.step_id or nil,
					session_id = admit_ctx.session and admit_ctx.session.session_id or nil,
					session_generation = admit_ctx.session and admit_ctx.session.session_epoch or nil,
				}
				out[#out + 1] = declaration
			end
		end
		return out
	end

	-- Bind the exact request declarations after request/build hooks. A hook may select or
	-- reorder declarations, but cannot replace or mutate a schema behind the validator.
	function tools.bind_declarations(_, _, list, opts)
		local llm_svc = D.svc.llm
		local default_model = (opts and opts.model) or (llm_svc and llm_svc:current()) or nil
		local bound, wire = {}, {}
		for _, declaration in ipairs(list or {}) do
			if type(declaration) == "table" and declaration.type == "hosted" then
				if type(declaration.name) ~= "string" or declaration.name == "" then
					error("hosted tool declaration requires a non-empty name")
				end
				wire[#wire + 1] = {
					type = "hosted",
					name = declaration.name,
					options = declaration.options,
				}
				goto continue_bind
			end
			local record = declarations[declaration]
			local bound_model = (opts and opts.model) or (record and record.model) or default_model
			local bound_mode = edit_mode(bound_model)
			local bound_step_id = (opts and opts.step_id ~= nil) and opts.step_id or (record and record.step_id) or nil
			local bind_opts = {
				step_id = bound_step_id,
				session_id = record and record.session_id or nil,
				session_generation = record and record.session_generation or nil,
			}
			local admit_ctx = admission_context(bound_model, bound_mode, bind_opts)
			if
				not record
				or registry[declaration.name] ~= record.definition
				or record.definition._version ~= record.version
				or not is_admitted(record.definition, bound_model, bound_mode, bind_opts, admit_ctx)
				or bound[declaration.name]
			then
				error("unregistered, changed or duplicate tool declaration: " .. tostring(declaration.name))
			end
			local ok, schema = pcall(mare.schema.compile, declaration.parameters)
			if not ok or schema:fingerprint() ~= record.schema:fingerprint() then
				error("changed or invalid tool schema: " .. tostring(declaration.name))
			end
			local token = {}
			bindings[token] = {
				schema = record.schema,
				definition = record.definition,
				version = record.version,
				model = bound_model,
				edit_mode = bound_mode,
				step_id = bound_step_id,
				session_id = admit_ctx.session and admit_ctx.session.session_id or record.session_id,
				session_generation = admit_ctx.session and admit_ctx.session.session_epoch or record.session_generation,
			}
			bound[declaration.name] = token
			local defer_loading = record.definition.exposure == "deferred" and native_tool_update(bound_model)
			wire[#wire + 1] = {
				name = declaration.name,
				description = declaration.description,
				parameters = record.schema:value(),
				defer_loading = defer_loading and true or nil,
			}
			::continue_bind::
		end
		return bound, wire
	end

	function tools.touch(_, _)
		generation = generation + 1
	end

	function tools.capabilities(_, _)
		return { result_pipeline = 1 }
	end

	return {
		tools = tools,
		registry = registry,
		native_tool_update = native_tool_update,
		deferred_names = deferred_names,
		excluded = excluded,
		is_admitted = is_admitted,
		resolve_binding = function(token)
			return bindings[token]
		end,
	}
end
