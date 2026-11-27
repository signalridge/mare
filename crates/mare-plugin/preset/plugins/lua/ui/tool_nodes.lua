-- ui.tool_nodes: tool-call nodes (exploration group, bash command, generic tool card);
-- part of the `ui` plugin (plugins/ui.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg, U = D.ctx, D.cfg, D.U

	local tool_node -- forward declaration (defined below)
	local result_text -- forward declaration (defined below)

	--- " (exit N)" for a failed command, " (failed)" for any other failed call, "" otherwise
	local function failure_note(res)
		if not (res and res.is_error) then
			return ""
		end
		local code = type(res.details) == "table" and tonumber(res.details.code) or nil
		if code and code ~= 0 then
			return (" (exit %d)"):format(code)
		end
		return " (failed)"
	end

	local function group_node(group, expanded)
		if expanded then
			local nodes = {}
			for _, it in ipairs(group.items) do
				nodes[#nodes + 1] = tool_node(it.call, it.res, true)
			end
			return ui.Lines(nodes)
		end
		local running, failed, partial = false, false, false
		for _, it in ipairs(group.items) do
			if not it.res then
				running = true
			elseif it.res.is_error then
				failed = true
			end
			if it.res and result_text(it.res):find("[sandbox: PARTIAL; unavailable: fs_deny_read]", 1, true) then
				partial = true
			end
		end
		-- merge consecutive calls of the same kind into one line ("Read a.rs, b.rs")
		local lines, prev = {}, nil
		local verbs = { Read = "Read", ListFiles = "List", Search = "Search" }
		for _, it in ipairs(group.items) do
			local label = it.label and tostring(it.label) or it.call.name
			local note = failure_note(it.res)
			if note ~= "" then
				-- a failed call gets a line of its own, in the error style, with its exit code
				prev = nil
				lines[#lines + 1] = { kind = it.kind, labels = { label }, note = note }
			elseif prev and prev.kind == it.kind and it.kind == "Read" then
				prev.labels[#prev.labels + 1] = label
			else
				prev = { kind = it.kind, labels = { label } }
				lines[#lines + 1] = prev
			end
		end
		local body = {}
		local hidden = math.max(0, #lines - cfg.group_lines)
		if hidden > 0 then
			local rest = {}
			for i = hidden + 1, #lines do
				rest[#rest + 1] = lines[i]
			end
			lines = rest
		end
		local ascii = U.icon_set and U.icon_set() == "ascii"
		for i, l in ipairs(lines) do
			body[#body + 1] = ui.Line({
				ui.Span(i == 1 and (ascii and "  `- " or "  └ ") or "    "):hl("MareToolGutter"),
				ui.Span(verbs[l.kind] .. " "):hl(l.note and "MareError" or "MareToolName"),
				ui.Span(table.concat(l.labels, ", ")):hl(l.note and "MareError" or "MareToolArgs"),
				ui.Span(l.note or ""):hl("MareError"),
			})
		end
		-- the header counts what was read / searched / listed (plugins/lua/ui/explore_summary.lua)
		local summary = require("ui.explore_summary").text(group.items, running)
		if partial then
			summary = summary .. (ascii and " - PARTIAL: fs_deny_read" or " · PARTIAL: fs_deny_read")
		end
		local head = running and U.ToolCall:running({ ui.Span(summary):hl("MareToolName") })
			or ui.Line({
				ui.Span(ascii and "* " or "● "):hl(failed and "MareToolError" or "MareToolOk"),
				ui.Span(summary):hl("MareToolName"),
			})
		if hidden > 0 then
			table.insert(
				body,
				1,
				ui.Line({
					ui.Span((ascii and "    ... %d earlier" or "    … %d earlier"):format(hidden)):hl("MareDim"),
				})
			)
		end
		table.insert(body, 1, head)
		return ui.Lines(body)
	end

	--- single command: "Running cmd" → "Ran cmd", wrapped lines prefixed " │ ",
	--- output clipped to its last lines with a "… N more lines" hint above them (ctrl+o / ctrl+t
	--- show everything); a failed command has the error style and its exit code
	local function bash_node(call, res, expanded, live)
		local cmd = (call.arguments or {}).command or ""
		local cmd_lines = {}
		for l in string.gmatch(cmd .. "\n", "(.-)\n") do
			cmd_lines[#cmd_lines + 1] = l
		end
		local state = res == nil and "running" or (res.is_error and "error" or "ok")
		local head_spans =
			{ ui.Span(state == "running" and "Running " or "Ran "):hl("MareToolName"), ui.Span(cmd_lines[1] or "") }
		if state == "error" then
			head_spans[#head_spans + 1] = ui.Span(failure_note(res)):hl("MareError")
		end
		local ascii = U.icon_set and U.icon_set() == "ascii"
		local out = {}
		out[1] = state == "running" and U.ToolCall:running(head_spans)
			or ui.Line({
				ui.Span(ascii and "* " or "● "):hl(state == "error" and "MareToolError" or "MareToolOk"),
				table.unpack(head_spans),
			})
		local max_cmd = expanded and #cmd_lines or math.min(#cmd_lines, cfg.preview_lines)
		for i = 2, max_cmd do
			out[#out + 1] =
				ui.Line({ ui.Span(ascii and "  | " or "  │ "):hl("MareToolGutter"), ui.Span(cmd_lines[i]) })
		end
		if max_cmd < #cmd_lines then
			local key = (mare.keymap.hint("toggle-expand", "editor") or {})[1]
			if key then
				local snapshot = D.service.service.get_keymap(nil, nil, "editor")
				for _, binding in ipairs(snapshot.bindings or {}) do
					if binding.key == key then
						key = binding.key_label or key
						break
					end
				end
			end
			out[#out + 1] = ui.Line({
				ui.Span(ascii and "  | " or "  │ "):hl("MareToolGutter"),
				ui.Span(
					((ascii and "... %d more line%s%s" or "… %d more line%s%s")):format(
						#cmd_lines - max_cmd,
						#cmd_lines - max_cmd == 1 and "" or "s",
						key and (" (" .. key .. " to expand)") or ""
					)
				):hl("MareDim"),
			})
		end
		local text
		if live ~= nil then
			text = live
		else
			text = (res and result_text(res)) or ""
			text = string.gsub(mare.text.sanitize(text), "%s+$", "")
		end
		if text ~= "" then
			local para =
				ui.Paragraph(ui.Line({ ui.Span(text):hl(state == "error" and "MareError" or "MareToolResult") }))
			-- the last lines, not the first: the end of a command's output is what tells
			-- how it went
			out[#out + 1] = U.ToolCall:body(expanded and para or ui.Clip(para, cfg.preview_lines, true))
		elseif res then
			out[#out + 1] = U.ToolCall:body(ui.Line({ ui.Span("(no output)"):hl("MareDim") }))
		end
		return ui.Lines(out)
	end

	local function tool_def(name)
		local tools_service = ctx:get("tools")
		return tools_service and tools_service:get(name)
	end

	--- Unwrap `call_tool` into the inner real tool call for display so UI cards and transcripts
	--- show the target tool as primary with `call_tool` only as a subtle source marker.
	local function unwrap_call_tool(call)
		if
			type(call) == "table"
			and call.name == "call_tool"
			and type(call.arguments) == "table"
			and type(call.arguments.name) == "string"
			and call.arguments.name ~= ""
		then
			local inner_args = call.arguments.arguments
			if type(inner_args) == "string" then
				local ok, decoded = pcall(mare.json.decode, inner_args)
				inner_args = (ok and type(decoded) == "table") and decoded or {}
			elseif type(inner_args) ~= "table" then
				inner_args = {}
			end
			return { id = call.id, name = call.arguments.name, arguments = inner_args }, "call_tool"
		end
		return call, nil
	end

	local function summary_of(call)
		local d = tool_def(call.name)
		if d and d.summary then
			local ok, s = pcall(d.summary, call.arguments or {})
			if ok and s then
				return s
			end
		end
		local args = call.arguments and mare.json.encode(call.arguments) or ""
		return ("%s(%s)"):format(call.name, mare.text.trunc_width(args, 60))
	end

	result_text = function(res)
		if type(res.content) == "string" then
			return res.content
		end
		local parts = {}
		for _, b in ipairs(res.content or {}) do
			if b.type == "text" then
				parts[#parts + 1] = b.text
			end
		end
		return table.concat(parts, "\n")
	end

	tool_node = function(call, res, expanded, live)
		local display_call, source = unwrap_call_tool(call)
		local info = {
			summary = summary_of(display_call),
			tool = tool_def(display_call.name),
			live = live,
			source = source,
		}
		if res then
			info.text = result_text(res)
		end
		local ok, node = pcall(U.ToolCall.render, U.ToolCall, display_call, res, expanded, info)
		if ok then
			if info.text and info.text:find("[sandbox: PARTIAL; unavailable: fs_deny_read]", 1, true) then
				return ui.Lines({ node, ui.Line({ ui.Span("  PARTIAL: fs_deny_read"):hl("MareError") }) })
			end
			return node
		end
		return ui.Line({
			ui.Span(
				((U.icon_set and U.icon_set() == "ascii") and "* " or "● ")
					.. info.summary
					.. "  (render error: "
					.. tostring(node)
					.. ")"
			):hl("MareError"),
		})
	end

	return {
		group_node = group_node,
		bash_node = bash_node,
		tool_node = tool_node,
		unwrap_call_tool = unwrap_call_tool,
	}
end
