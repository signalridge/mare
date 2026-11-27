-- Shell execution and transcript rendering, loaded only when a shell command is submitted.
return function(ctx, cfg, running, mark_end)
	local U = mare.ui
	local function show(cmd, out, code, private)
		if not U.connected() then
			io.stderr:write(out, "\n")
			return
		end
		-- Shell mode: the command shows as "! cmd", output hangs below it
		local head = ui.Paragraph(
			ui.Line({
				ui.Span(private and "!! " or "! "):hl(code == 0 and "MareAccent" or "MareError"),
				ui.Span(cmd):hl("MareToolName"),
				ui.Span(private and "  (not added to context)" or ""):hl("MareDim"),
			}),
			{ ui.Span("  ") }
		)
		local text = string.gsub(out, "%s+$", "")
		if text == "" then
			text = "(no output)"
		end
		local function node(expanded)
			local body = U.ToolCall:result_text(text, code ~= 0, true)
			if not expanded then
				body = ui.Clip(body, cfg.preview_lines)
			end
			return ui.Lines({ head, U.ToolCall:body(body) })
		end
		U.finish(U.push("tool", { node = node(false), expanded = node(true) }))
	end

	-- `tok` is registered by the caller before "Running …" is shown: an Esc as soon as that
	-- line appears must find the command abortable (and be consumed, not half a double-Esc)
	local function run(cmd, tok)
		local ok, prog, args, input = pcall(ctx.tools.shell_command, ctx.tools, cmd)
		if not ok then
			running[tok] = nil
			return tostring(prog), -1
		end
		if tok:is_cancelled() then
			running[tok] = nil
			return "[command aborted]", -1
		end
		local pol = ctx.tools.env_policy and ctx.tools:env_policy() or {}
		local cwd = ctx.tools.cwd and ctx.tools:cwd() or nil
		local okr, r = pcall(mare.proc.run, {
			cmd = prog,
			args = args,
			cwd = cwd,
			input = input,
			stderr = "merge",
			timeout_ms = cfg.timeout_ms,
			env = ctx.tools:process_env(),
			env_include = pol.env_include,
			env_exclude = pol.env_exclude,
			cancel = tok,
		})
		running[tok] = nil
		mark_end()
		if not okr then
			return tostring(r), -1
		end
		local out = mare.text.sanitize(r.stdout or "")
		if r.timed_out then
			out = out .. "\n[command timed out]"
		elseif r.cancelled then
			out = out .. "\n[command aborted]"
		end
		return out, r.code or -1
	end

	return run, show
end
