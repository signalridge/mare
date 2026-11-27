-- Proxy tool for invoking deferred tools on models without native mid-conversation
-- tool updates (model.tool_update == nil). Keeps the top-level tools[] array fixed
-- across turns while routing inner execution through tools:execute (schema validation,
-- hooks, permissions, approval, sandbox, and persistence).
return function(D)
	local ctx = D.ctx
	local tools, registry = D.registry.tools, D.registry.registry
	local excluded = D.registry.excluded
	local tool_allowed = mare.subagent and mare.subagent.tool_allowed

	local function is_plain_object(t)
		return type(t) == "table" and not mare.json.is_array(t) and (#t == 0 or next(t, #t) ~= nil)
	end

	local function decode_arguments(raw)
		if raw == nil then
			return {}
		end
		if type(raw) == "table" then
			if is_plain_object(raw) then
				return raw
			end
			return nil, "Invalid call_tool arguments: expected a JSON object"
		end
		if type(raw) == "string" then
			local trimmed = raw:match("^%s*(.-)%s*$")
			if trimmed == "" then
				return {}
			end
			local ok, decoded = pcall(mare.json.decode_lossless, trimmed)
			if ok and is_plain_object(decoded) then
				return decoded
			end
			return nil, "Invalid call_tool arguments: expected a JSON object"
		end
		return nil, "Invalid call_tool arguments: expected a JSON object"
	end

	return {
		rewrite = function(call)
			local decoded = decode_arguments(call.arguments.arguments)
			if not decoded then
				return nil
			end
			local next_args = table.copy(call.arguments)
			next_args.arguments = decoded
			return next_args
		end,
		concurrency_safe = function(args)
			if type(args) ~= "table" or type(args.name) ~= "string" or args.name == "" then
				return false
			end
			local target = registry[args.name]
			if not target or target.exposure ~= "deferred" then
				return false
			end
			local inner_args = decode_arguments(args.arguments)
			if not inner_args then
				return false
			end
			return tools.concurrency_safe(tools, ctx, { name = args.name, arguments = inner_args })
		end,
		summary = function(args)
			local name = type(args) == "table" and type(args.name) == "string" and args.name or ""
			if name ~= "" then
				local inner_args = decode_arguments(args.arguments) or {}
				local target = registry[name]
				if target and target.summary then
					local ok, s = pcall(target.summary, inner_args)
					if ok and s then
						return s
					end
				end
				return ("%s(%s)"):format(name, mare.text.trunc_width(mare.json.encode(inner_args), 60))
			end
			return "call_tool()"
		end,
		execute = function(args, tctx)
			local target_name = type(args) == "table" and type(args.name) == "string" and args.name or ""
			if target_name == "" or target_name == "call_tool" or target_name == "tool_search" then
				return { content = "Invalid deferred tool name: " .. tostring(target_name), is_error = true }
			end
			if (tool_allowed and not tool_allowed(target_name)) or excluded(target_name) then
				return { content = "Unknown or unavailable deferred tool: " .. target_name, is_error = true }
			end
			local def = registry[target_name]
			if not def then
				return { content = "Unknown deferred tool: " .. target_name, is_error = true }
			end
			if def.exposure ~= "deferred" then
				if def.exposure == "nested" then
					return {
						content = ("Tool %q cannot be invoked through call_tool."):format(target_name),
						is_error = true,
					}
				end
				return {
					content = ("Tool %q is not a deferred tool; call it directly instead of through call_tool."):format(
						target_name
					),
					is_error = true,
				}
			end
			local inner_args, err = decode_arguments(args.arguments)
			if not inner_args then
				return { content = err, is_error = true }
			end
			local decls = tools.definitions(tools, ctx, tctx.model, { target_name }, { step_id = tctx.step_id })
			local llm = D.svc and D.svc.llm
			if #decls > 0 and llm and require("mare_lib").callable(llm.constrain_request) then
				local sess = D.svc.session and D.svc.session:info() or nil
				local constrained = llm:constrain_request({
					model = tctx.model,
					session_id = sess and sess.session_id or nil,
					tools = decls,
				})
				if type(constrained) == "table" and type(constrained.tools) == "table" then
					decls = constrained.tools
				end
			end
			if #decls == 0 then
				return {
					content = ("Deferred tool %q is not available in this context."):format(target_name),
					is_error = true,
				}
			end
			local invoke = tctx.bind_tools(decls, {
				max_calls = 1,
				max_parallel = 1,
				audience = "model",
				on_update = tctx.update,
				origin = "call_tool",
				model = tctx.model,
				step_id = tctx.step_id,
			})
			return invoke(target_name, inner_args)
		end,
	}
end
