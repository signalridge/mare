-- Shell execution and transcript rendering, loaded only when a shell command is submitted.
return function(ctx, cfg, running, mark_end, retained)
	local U = mare.ui
	local SPILL_PREFIX = "shell"
	local PREVIEW_MAX_BYTES = 4096
	local RETAINED_MAX_BYTES = 512 * 1024

	local function preview_slice(text, max_lines, max_bytes)
		max_lines = math.max(1, tonumber(max_lines) or 5)
		max_bytes = math.max(256, tonumber(max_bytes) or PREVIEW_MAX_BYTES)
		local len = #text
		local lines = {}
		local total_lines = 0
		local pos = 1
		local used = 0
		local truncated = false
		while pos <= len do
			local nl = string.find(text, "\n", pos, true)
			local line_end = nl and (nl - 1) or len
			total_lines = total_lines + 1
			if not truncated and #lines < max_lines then
				local chunk = string.sub(text, pos, line_end)
				local need = #chunk + (#lines > 0 and 1 or 0)
				if used + need > max_bytes then
					local rem = math.max(0, max_bytes - used - (#lines > 0 and 1 or 0))
					if rem > 0 then
						lines[#lines + 1] = mare.text.cut(chunk, rem)
					end
					truncated = true
				else
					lines[#lines + 1] = chunk
					used = used + need
				end
			else
				truncated = true
			end
			if not nl then
				break
			end
			pos = nl + 1
		end
		local extra_lines = math.max(0, total_lines - #lines)
		return table.concat(lines, "\n"), truncated or extra_lines > 0, extra_lines, total_lines
	end

	local function spill_output(text)
		local state_dir = mare.paths and mare.paths.state
		if type(state_dir) ~= "string" or state_dir == "" then
			return nil
		end
		local pid = mare.pid or 0
		local st = mare.proc and mare.proc.start_time and mare.proc.start_time(pid) or 0
		local path = ("%s/spill/%s-%s-%s-%s.log"):format(
			state_dir,
			SPILL_PREFIX,
			tostring(pid),
			tostring(st),
			mare.crypto.random_hex(6)
		)
		local ok = pcall(mare.proc.spill_write, SPILL_PREFIX, path, text, false)
		return ok and path or nil
	end

	local function expand_hint_suffix()
		local keys = mare.keymap and mare.keymap.hint and mare.keymap.hint("toggle-expand", "editor") or nil
		local key = type(keys) == "table" and keys[1] or nil
		if type(key) ~= "string" or key == "" then
			return ""
		end
		local ui_svc = ctx and ctx.get and ctx:get("ui") or nil
		if ui_svc and ui_svc.get_keymap then
			local ok, snapshot = pcall(ui_svc.get_keymap, ui_svc, "editor")
			if ok and type(snapshot) == "table" then
				for _, binding in ipairs(snapshot.bindings or {}) do
					if binding.key == key then
						key = binding.key_label or key
						break
					end
				end
			end
		end
		return (" (%s to expand)"):format(key)
	end

	local function build_node(head, body_text, code, footer, clip_lines)
		local body = U.ToolCall:result_text(body_text, code ~= 0, true)
		if clip_lines then
			body = ui.Clip(body, clip_lines)
		end
		if footer then
			body = ui.Lines({ body, ui.Line({ ui.Span(footer):hl("MareDim") }) })
		end
		return ui.Lines({ head, U.ToolCall:body(body) })
	end

	local function evict_retained(max_blocks, max_bytes)
		if type(retained) ~= "table" then
			return
		end
		local total_bytes = 0
		for _, item in ipairs(retained) do
			total_bytes = total_bytes + (item.bytes or 0)
		end
		while #retained > max_blocks or (#retained > 1 and total_bytes > max_bytes) do
			local old = table.remove(retained, 1)
			total_bytes = total_bytes - (old.bytes or 0)
			local spill = old.spill or (old.text and spill_output(old.text)) or nil
			old.text = nil
			local footer = spill
					and ("[output spilled to free memory (%d lines, %d bytes): %s]"):format(
						old.total_lines or 0,
						old.total_bytes or 0,
						spill
					)
				or ("[output truncated to free memory (%d lines, %d bytes)]"):format(
					old.total_lines or 0,
					old.total_bytes or 0
				)
			pcall(U.update, old.id, { expanded = build_node(old.head, old.preview, old.code, footer, nil) })
		end
	end

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
		local max_preview_lines = math.max(1, tonumber(cfg.preview_lines) or 5)
		local preview, overflow, extra_lines, total_lines = preview_slice(text, max_preview_lines, PREVIEW_MAX_BYTES)
		-- Always wrap the byte-bounded preview in visual-row `ui.Clip` so a long single-line
		-- output without newlines still collapses to `preview_lines` visual rows on narrow screens.
		if not overflow then
			local collapsed_node = build_node(head, preview, code, nil, max_preview_lines)
			-- Tiny outputs that cannot wrap beyond `preview_lines` rows need no separate expanded node.
			if #text <= max_preview_lines * 20 then
				U.finish(U.push("tool", { node = collapsed_node }))
				return
			end
			local expanded_node = build_node(head, text, code, nil, nil)
			local id = U.push("tool", { node = collapsed_node, expanded = expanded_node })
			U.finish(id)
			if type(retained) == "table" then
				local max_inline = math.max(PREVIEW_MAX_BYTES, tonumber(cfg.expanded_max_bytes) or (64 * 1024))
				local max_blocks = math.max(1, tonumber(cfg.retained_blocks) or 20)
				retained[#retained + 1] = {
					id = id,
					head = head,
					preview = preview,
					total_lines = total_lines,
					total_bytes = #text,
					code = code,
					text = text,
					bytes = #text,
					spill = nil,
				}
				evict_retained(max_blocks, math.max(max_inline, RETAINED_MAX_BYTES))
			end
			return
		end
		local hint_suffix = expand_hint_suffix()
		local collapsed_footer = extra_lines > 0
				and ("… %d more line%s%s"):format(extra_lines, extra_lines == 1 and "" or "s", hint_suffix)
			or ("…" .. hint_suffix)
		local collapsed_node = build_node(head, preview, code, collapsed_footer, max_preview_lines)

		local max_inline = math.max(PREVIEW_MAX_BYTES, tonumber(cfg.expanded_max_bytes) or (64 * 1024))
		local max_blocks = math.max(1, tonumber(cfg.retained_blocks) or 20)
		local expanded_node
		local spill_path
		local keep_inline = #text <= max_inline
		local retained_bytes
		if keep_inline then
			expanded_node = build_node(head, text, code, nil, nil)
			retained_bytes = #text
		else
			spill_path = spill_output(text)
			local head_text = mare.text.cut(text, max_inline)
			local footer = spill_path
					and ("[full output (%d lines, %d bytes): %s]"):format(total_lines, #text, spill_path)
				or ("[output truncated (%d lines, %d bytes)]"):format(total_lines, #text)
			expanded_node = build_node(head, head_text, code, footer, nil)
			retained_bytes = #head_text
		end

		local id = U.push("tool", { node = collapsed_node, expanded = expanded_node })
		U.finish(id)
		if type(retained) == "table" then
			retained[#retained + 1] = {
				id = id,
				head = head,
				preview = preview,
				total_lines = total_lines,
				total_bytes = #text,
				code = code,
				text = keep_inline and text or nil,
				bytes = retained_bytes,
				spill = spill_path,
			}
			evict_retained(max_blocks, math.max(max_inline, RETAINED_MAX_BYTES))
		end
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
