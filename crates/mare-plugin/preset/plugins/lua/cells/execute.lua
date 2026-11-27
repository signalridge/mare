-- Build the capability table inside execute so its bridges inherit the active tool context.
return function(ctx, cfg, args, tctx, memory, pool)
	if #args.code == 0 or #args.code > 65536 then
		return { content = "Lua source exceeds 65536 bytes", is_error = true }
	end
	local model = tctx.model or ctx.llm:current()
	local declarations = ctx.tools:definitions(model, args.capabilities or {}, { step_id = tctx.step_id })
	local selected, seen, metadata, descriptors = {}, {}, {}, {}
	local describe = require("cells.sdk")()
	for _, d in ipairs(declarations) do
		local definition = ctx.tools:get(d.name)
		if definition.allow_nested ~= false and not seen[d.name] then
			seen[d.name] = true
			selected[#selected + 1] = d
			-- Schemas are omitted from ALL_TOOLS metadata.
			metadata[#metadata + 1] =
				mare.json.decode_lossless(mare.json.encode_strict({ name = d.name, description = d.description }))
			descriptors[d.name] = { declaration = d, definition = definition }
		end
	end
	local cancel = tctx.cancel:child()
	local pending = {}
	local function call(fn)
		local task = mare.task.spawn(fn)
		pending[task] = true
		local ok, result = pcall(task.wait, task)
		pending[task] = nil
		if not ok then
			error(result, 0)
		end
		return result
	end
	local cancel_guard <close> = setmetatable({}, {
		__close = function()
			cancel:cancel()
		end,
	})
	local timeout = args.timeout_ms
	if timeout == nil then
		timeout = cfg.timeout_ms
	end
	local max_calls = false
	if timeout ~= false then
		max_calls = math.floor(timeout / 60000 * (cfg.max_sub_calls_per_minute or 1024))
	end
	local invoke, batch = tctx.bind_tools(
		selected,
		{ cancel = cancel, max_calls = max_calls, max_parallel = cfg.max_parallel_sub_calls or 10 }
	)
	local tools = {}
	for _, d in ipairs(selected) do
		local name = d.name
		tools[name] = function(arguments)
			if tctx.cancel:is_cancelled() then
				error("script cancelled", 0)
			end
			local result = call(function()
				return invoke(name, arguments)
			end)
			if result.is_error then
				error(type(result.content) == "string" and result.content or mare.json.encode_strict(result.content), 0)
			end
			-- Only JSON crosses a VM boundary; UI nodes and executable callbacks stay in host.
			if result.value ~= nil then
				return result.value
			end
			return { content = result.content, details = result.details }
		end
	end
	local vm
	local helpers = require("cells.helpers")(cfg, memory, nil, args)
	local expose = {}
	for name, value in pairs(helpers.expose) do
		expose[name] = value
	end
	local deadline = timeout == false and math.huge or (mare.task.clock() + timeout)
	local retained, release, owner_lost = pool.acquire(args.state_owner, cancel, deadline)
	local pool_guard <close> = setmetatable({}, { __close = release })
	local run_options = {
		timeout_ms = timeout,
		cancel = cancel,
		memory_bytes = cfg.memory_bytes,
		max_bytes = cfg.max_bytes,
		max_message_bytes = cfg.max_message_bytes ~= 0 and cfg.max_message_bytes or cfg.max_bytes,
		max_output_bytes = cfg.max_output_bytes ~= 0 and cfg.max_output_bytes or cfg.max_bytes,
		max_pending_calls = cfg.max_pending_calls,
		stop_grace_ms = cfg.stop_grace_ms,
		grace_ms = cfg.grace_ms,
		max_instructions = cfg.max_instructions,
		on_output = helpers.print,
		expose = {
			text = expose.text,
			notify = expose.notify,
			yield_control = expose.yield_control,
			image = expose.image,
			audio = expose.audio,
			generatedImage = expose.generatedImage,
			store = expose.store,
			load = expose.load,
			exit = expose.exit,
			tools = tools,
			parallel = function(calls, options)
				local results = call(function()
					return batch(calls)
				end)
				local values = mare.json.array()
				for index, result in ipairs(results) do
					local value
					if result.value ~= nil then
						value = result.value
					else
						value = { content = result.content, details = result.details }
					end
					if options and options.settled then
						values[index] = result.is_error and { status = "rejected", reason = result.content }
							or { status = "fulfilled", value = value }
					else
						if result.is_error then
							error(
								type(result.content) == "string" and result.content
									or mare.json.encode_strict(result.content),
								0
							)
						end
						values[index] = value
					end
				end
				return values
			end,
			ALL_TOOLS = metadata,
			describe_tool = function(name)
				local descriptor = descriptors[name]
				if descriptor then
					return describe(descriptor.declaration, descriptor.definition)
				end
				return mare.json.null
			end,
		},
	}
	vm = retained or mare.sandbox.new(run_options)
	if timeout ~= false then
		run_options.timeout_ms = math.max(1, math.floor(deadline - mare.task.clock()))
	end
	local result, err, status = vm:run(args.code, run_options)
	cancel:cancel()
	local deadline = mare.task.clock() + 5000
	for task in pairs(pending) do
		local confirmed, ok, result = mare.task.timeout(math.max(1, deadline - mare.task.clock()), function()
			return pcall(task.wait, task)
		end)
		local unconfirmed = ok
			and type(result) == "table"
			and (result.stop_unconfirmed or (result.details and result.details.stop_unconfirmed))
		if not confirmed or unconfirmed then
			local settled =
				helpers.settle(nil, "Nested tool stop is unconfirmed; completed effects were not rolled back.")
			local content = table.copy(helpers.items)
			content[#content + 1] = { type = "text", text = settled.error }
			return {
				content = content,
				is_error = true,
				details = {
					stop_unconfirmed = true,
					output = helpers.output(),
					output_items = helpers.items,
					error = settled.error,
					is_error = true,
					cancelled = tctx.cancel:is_cancelled(),
				},
			}
		end
	end
	local exited = helpers.exited()
	local interrupted = tctx.cancel:is_cancelled()
		or (status and status.cancelled)
		or (err and tostring(err):find("time limit", 1, true))
	helpers.finish(tctx.cancel:is_cancelled() or (not exited and interrupted))
	if exited and not tctx.cancel:is_cancelled() then
		result = { result = mare.json.null, output = status and status.output or "" }
	end
	if not result then
		local settled = helpers.settle(nil, tostring(err))
		err = settled.error
		return {
			content = (function()
				local content = table.copy(helpers.items)
				content[#content + 1] = {
					type = "text",
					text = tostring(err),
				}
				return content
			end)(),
			is_error = true,
			details = {
				output = helpers.output(),
				output_items = helpers.items,
				error = tostring(err),
				cancelled = status and status.cancelled or false,
				stop_unconfirmed = status and status.stop_unconfirmed or false,
				is_error = true,
			},
		}
	end
	local appended, append_error = pcall(function()
		if result.state_lost or owner_lost then
			helpers.print(
				"[retained program environment was lost; variables were cleared; completed tool effects remain]\n"
			)
		end
		helpers.append_return(result.result)
	end)
	local settled = helpers.settle(result.result, not appended and tostring(append_error) or nil)
	if settled.error then
		local failed_content = table.copy(helpers.items)
		failed_content[#failed_content + 1] = { type = "text", text = settled.error }
		return {
			content = failed_content,
			is_error = true,
			details = {
				output = helpers.output(),
				output_items = helpers.items,
				error = settled.error,
				is_error = true,
				cancelled = false,
				stop_unconfirmed = false,
			},
		}
	end
	local content = helpers.items
	if #content == 0 then
		content = { { type = "text", text = "Script completed (no output)." } }
	end
	return {
		content = content,
		details = {
			result = result.result,
			output = helpers.output(),
			output_items = helpers.items,
			cancelled = false,
			is_error = false,
		},
	}
end
