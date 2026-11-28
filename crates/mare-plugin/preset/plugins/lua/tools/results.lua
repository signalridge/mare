-- Policies settle canonical values before pure model projection and retention; logs never rewrite values.
return function(D)
	local ctx, cfg = D.ctx, D.cfg
	local MAX_LINES, MAX_BYTES = cfg.max_lines, cfg.max_bytes
	local truncate_utf8 = require("mare_lib").truncate_utf8
	local function truncate_result(res)
		if type(res.output_notice) == "string" and type(res.content) == "string" then
			res.content = mare.text.retain(
				res.content,
				{ max_bytes = MAX_BYTES, max_lines = MAX_LINES, suffix = res.output_notice }
			)
			return res
		end
		if res.no_truncate then
			return res
		end
		local content = res.content
		if type(content) == "string" then
			local text, cut = truncate_utf8(content, {
				max_lines = MAX_LINES,
				max_bytes = MAX_BYTES,
				max_line_chars = res.max_line_chars,
			})
			res.content = text
			res.truncated = cut or nil
		elseif type(content) == "table" then
			local lines_left, bytes_left = MAX_LINES, MAX_BYTES
			for _, b in ipairs(content) do
				if type(b) == "table" and b.type == "text" and type(b.text) == "string" then
					if lines_left <= 0 or bytes_left <= 0 then
						b.text = "[output truncated]"
						res.truncated = true
					else
						local text, cut = truncate_utf8(b.text, {
							max_lines = lines_left,
							max_bytes = bytes_left,
							max_line_chars = res.max_line_chars,
						})
						b.text = text
						res.truncated = res.truncated or cut or nil
						local _, nl = string.gsub(text, "\n", "")
						lines_left = lines_left - nl - 1
						bytes_left = bytes_left - #text
					end
				end
			end
		end
		return res
	end
	local function clone_json(v)
		if v == nil then
			return nil
		end
		return mare.json.clone(v)
	end
	local function detach(result)
		if type(result) == "string" then
			result = { content = result }
		end
		result = result or { content = "" }
		return mare.json.clone({
			content = result.content,
			details = result.details,
			value = result.value,
			is_error = result.is_error,
			error_code = result.error_code,
			cancelled = result.cancelled,
			terminate = result.terminate,
			stop_unconfirmed = result.stop_unconfirmed,
			sandbox_denials = result.sandbox_denials,
		})
	end
	local function validate(result, definition)
		if not result.is_error and definition._output_validator then
			local ok, error = pcall(definition._output_validator.validate, definition._output_validator, result.value)
			if not ok then
				return { content = "Invalid tool program value: " .. tostring(error), is_error = true }
			end
		end
		return result
	end
	local function finish(raw, captured, definition, call, opts, operation)
		local program = validate(captured or detach(raw), definition)
		local function evidence()
			return raw.sandbox_denials and clone_json(raw.sandbox_denials) or nil
		end
		local wf_prog = ctx.bus:waterfall("tool/program-result", {
			result = program,
			call = clone_json(call),
			operation = table.copy(operation),
		})
		if type(wf_prog) == "table" and wf_prog.result ~= nil then
			program = wf_prog.result
		end
		program = validate(detach(program), definition)
		program.sandbox_denials = evidence()
		local display = detach(program)
		for _, key in ipairs({ "display", "no_truncate", "max_line_chars", "output_notice" }) do
			display[key] = raw[key]
		end
		if program.value ~= nil then
			local ok, content = pcall(function()
				local value = definition.render_value
						and definition.render_value(
							clone_json(call.arguments),
							clone_json(program.value),
							{ audience = opts.audience or "model" }
						)
					or mare.json.encode_strict(program.value)
				assert(
					type(value) == "string" or type(value) == "table",
					"model projection must return text or content blocks"
				)
				if type(value) == "table" then
					local cloned = clone_json(value)
					assert(type(cloned) == "table", "model projection content must be an array")
					if not mare.json.is_array(cloned) and next(cloned) == nil then
						cloned = mare.json.array(cloned)
					end
					assert(mare.json.is_array(cloned), "model projection content must be an array")
					value = cloned
				end
				return value
			end)
			if ok then
				display.content = content
			else
				display.content = "Tool output presentation failed: " .. tostring(content)
				display.is_error = true
				display.error_code = "presentation_failed"
				display.details = display.details or {}
				display.details.presentation_error = tostring(content)
			end
		end
		display.value = nil -- canonical data is not a model/log payload
		local previous = display
		local wf_after = ctx.bus:waterfall("tool/after", {
			result = display,
			call = clone_json(call),
			operation = table.copy(operation),
		})
		if type(wf_after) == "table" and wf_after.result ~= nil then
			display = wf_after.result
		elseif type(wf_after) == "string" then
			previous.content = wf_after
			display = previous
		end
		if type(display) == "string" then
			previous.content = display
			display = previous
		end
		assert(type(display) == "table", "tool/after must return a ToolResult")
		display.sandbox_denials = evidence()
		local retention = table.copy(operation)
		retention.audience = opts.audience or "model"
		retention.max_bytes = math.min(cfg.max_bytes, operation.max_bytes or cfg.max_bytes)
		retention.max_lines = cfg.max_lines
		local wf_ok, wf_res = pcall(ctx.bus.waterfall_checked, ctx.bus, "tool/result", {
			result = display,
			call = clone_json(call),
			operation = retention,
		})
		if not wf_ok then
			pcall(mare.log, "warn", ("tool/result post-processing failed: %s"):format(tostring(wf_res)))
			display = {
				content = "Tool result post-processing failed.",
				is_error = true,
				error_code = "tool_result_failed",
			}
		elseif type(wf_res) == "table" and wf_res.result ~= nil then
			display = wf_res.result
		end
		display = truncate_result(display)
		display.terminate = program.terminate
		if
			type(raw) == "table"
			and type(raw._mare_changes) == "table"
			and display.error_code ~= "tool_result_failed"
		then
			display._mare_changes = clone_json(raw._mare_changes)
		end
		return display, program
	end
	local function coded_failure(err)
		local code, why = tostring(err):match("(SANDBOX_%u[%u_]*):[ \t]*([^\r\n]*)")
		if not code then
			return nil
		end
		why = why:gsub("%s+$", "")
		return { content = code .. ": " .. why, is_error = true, error_code = code:lower() }
	end
	local function report_partial(res, deny_read_partial)
		if not deny_read_partial then
			return res
		end
		local note = "[sandbox: PARTIAL; unavailable: fs_deny_read]"
		local content = res.content
		if type(content) == "string" then
			if not content:find(note, 1, true) then
				res.content = content .. "\n" .. note
			end
		elseif type(content) == "table" then
			local has_note = false
			for _, block in ipairs(content) do
				if type(block) == "table" and type(block.text) == "string" and block.text:find(note, 1, true) then
					has_note = true
				end
			end
			if not has_note then
				table.insert(content, 1, { type = "text", text = note })
			end
		else
			res.content = note
		end
		return res
	end
	local function validate_route_and_state(opts, call)
		local route_desc, route_snapshot
		if opts.route_snapshot ~= nil then
			local llm = D.svc.llm
			if not llm or not llm.validate_route_snapshot then
				return false,
					{
						content = "Tool call blocked: route snapshot cannot be verified without llm service",
						is_error = true,
						refused = true,
					}
			end
			local desc, rerr = llm:validate_route_snapshot(opts.route_snapshot)
			if not desc then
				return false,
					{
						content = "Tool call blocked: " .. tostring(rerr or "invalid or expired route snapshot"),
						is_error = true,
						refused = true,
					}
			end
			route_desc = desc
			route_snapshot = opts.route_snapshot
		end
		if
			(opts.on_state ~= nil and type(opts.on_state) ~= "function")
			or (
				opts.on_state == nil
				and opts.lifetime ~= "ephemeral-read-only"
				and not ((mare.args and mare.args.no_session) or mare.mode == "child")
			)
		then
			return false,
				{
					content = "Not run: persistent sessions require an on_state persistence callback for tool execution.",
					is_error = true,
					error_code = "persist_failed",
					refused = true,
				}
		end
		return true, route_desc, route_snapshot
	end
	local function persist_state(on_state, info, state, call)
		if not on_state then
			return true
		end
		local ok, err = on_state(call, state, info)
		if ok then
			return true
		end
		return false,
			{
				content = "Not run: the session can't be saved ("
					.. tostring(err)
					.. "); no tool runs until session writes work again.",
				is_error = true,
				error_code = "persist_failed",
				refused = true,
			}
	end
	return {
		program = detach,
		finish = finish,
		clone = clone_json,
		coded_failure = coded_failure,
		report_partial = report_partial,
		validate_route_and_state = validate_route_and_state,
		persist_state = persist_state,
	}
end
