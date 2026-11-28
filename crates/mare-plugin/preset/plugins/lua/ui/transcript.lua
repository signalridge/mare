-- ui.transcript: agent events -> transcript blocks (streaming, tools, turn end, replay);
-- part of the `ui` plugin (plugins/ui.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg, U = D.ctx, D.cfg, D.U
	local classify = D.classify.classify
	local W = require("ui.working")
	local group_node, bash_node, tool_node, unwrap_call_tool =
		D.tool_nodes.group_node, D.tool_nodes.bash_node, D.tool_nodes.tool_node, D.tool_nodes.unwrap_call_tool

	local handlers = {}
	local function on(ev_name, fn)
		if D.lazy_dispatch then
			local list = handlers[ev_name]
			if not list then
				list = {}
				handlers[ev_name] = list
			end
			list[#list + 1] = fn
		else
			ctx:on(ev_name, fn)
		end
	end
	local function dispatch(ev_name, ev)
		local list = handlers[ev_name]
		if not list then
			return
		end
		for i = 1, #list do
			list[i](ev)
		end
	end

	---------------------------------------------------------------------------
	-- transcript rendering
	---------------------------------------------------------------------------
	local cur = {
		text = nil,
		reasoning = nil,
		reasoning_start = 0,
		tools = {},
		out_chars = 0,
		last_working = 0,
		last_working_label = nil,
	}

	-- Bounded live preview window: collapsed live tool previews only display `preview_lines`
	-- rows, so copying/sanitizing/wrapping multi-MB output every 100ms stalls the UI thread.
	local LIVE_TAIL_MAX_BYTES = 8192
	local function live_tail_lines()
		return math.max(8, (cfg.preview_lines or 2) + 6)
	end
	local function slice_tail(s, max_lines, max_bytes)
		if type(s) ~= "string" or s == "" then
			return ""
		end
		local truncated = #s > max_bytes
		local win = truncated and string.sub(s, -max_bytes) or s
		local scan_end = string.byte(win, -1) == 10 and (#win - 1) or #win
		local lines, start = 0, 1
		for i = scan_end, 1, -1 do
			if string.byte(win, i) == 10 then
				lines = lines + 1
				if lines >= max_lines then
					start = i + 1
					break
				end
			end
		end
		if start == 1 and truncated then
			while start <= #win do
				local b = string.byte(win, start)
				if b < 128 or b >= 192 then
					break
				end
				start = start + 1
			end
		end
		return start > 1 and string.sub(win, start) or win
	end

	local function cancel_tool_flush(t)
		if not t then
			return
		end
		t.done = true
		t.pending_flush = false
		t.pending_partial = nil
		t.flush_gen = (t.flush_gen or 0) + 1
	end

	local function flush_tool_output(t)
		if not t or t.done or t.item then
			return
		end
		t.last = mare.task.clock()
		local raw_out
		if t.pending_partial ~= nil then
			raw_out = slice_tail(t.pending_partial, live_tail_lines(), LIVE_TAIL_MAX_BYTES)
			t.pending_partial = nil
		elseif t.output_buf then
			raw_out = t.output_buf:tail(live_tail_lines(), LIVE_TAIL_MAX_BYTES)
		else
			return
		end
		local out = string.gsub(mare.text.sanitize(raw_out), "%s+$", "")
		local node = t.call.name == "bash" and bash_node(t.call, nil, false, out) or tool_node(t.call, nil, false, out)
		U.update(t.id, { node = node })
	end
	local presentation_service = D.service.service
	local presentation_views, turn_documents = {}, {}
	local attempt_blocks, attempt_presentations = {}, {}
	local function present_text(id, source, base_node, replay)
		local key = tostring(id)
		local view = presentation_views[key]
		if not view then
			local ok, marker = pcall(U.AssistantMessage.marker, U.AssistantMessage)
			if not ok or type(marker) ~= "string" or utf8.len(marker) == nil then
				marker = "● "
			end
			view = { projected = base_node ~= nil, marker = marker, rest = string.rep(" ", mare.text.width(marker)) }
			presentation_views[key] = view
			if not replay then
				turn_documents[key] = true
				attempt_presentations[key] = true
			end
		end
		if base_node then
			local ok, accepted = pcall(U.validate_node, base_node)
			local copied = pcall(U.snapshot_node, base_node)
			if not ok or not accepted or not copied then
				base_node = nil
			end
		end
		local function source_node(text)
			return base_node or U.Indent({ U.Span(view.marker):hl("MareAssistantMarker") }, U.Markdown(text), view.rest)
		end
		local function update(node, _, snapshot)
			if view.projected or snapshot.enhanced then
				U.update(id, { node = node, expanded = snapshot.expanded_node or source_node(snapshot.source) })
				view.projected = true
			end
		end
		local ok, snapshot = pcall(D.native_markdown_document, ctx, key, source, {
			replay = replay == true,
			base_node = source_node(source),
			on_update = update,
		})
		if not ok then
			if not view.warned then
				ctx.logger:warn("Markdown presentation unavailable: " .. tostring(snapshot))
				view.warned = true
			end
			if view.projected then
				U.update(id, { node = source_node(source), expanded = source_node(source) })
			end
			return
		end
		if base_node then
			view.projected = true
		end
		update(snapshot.node, snapshot.revision, snapshot)
	end

	local function user_text(m)
		if type(m.content) == "string" then
			return m.content
		end
		local parts = {}
		for _, b in ipairs(m.content or {}) do
			if b.type == "text" then
				parts[#parts + 1] = b.text
			end
		end
		return table.concat(parts, "\n")
	end

	local function user_image_nodes(m)
		if type(m) ~= "table" or type(m.content) ~= "table" then
			return {}
		end
		local out = {}
		for _, b in ipairs(m.content) do
			if type(b) == "table" and b.type == "image" then
				local path = type(b.path) == "string" and b.path ~= "" and b.path or nil
				if b.data then
					local ok, bytes = pcall(mare.crypto.base64_decode, b.data)
					if ok and bytes then
						out[#out + 1] = ui.image({ bytes = bytes, path = path, max_rows = 12, max_cols = 60 })
					end
				elseif path then
					out[#out + 1] = ui.image({ path = path, max_rows = 12, max_cols = 60 })
				end
			end
		end
		return out
	end

	local group = nil -- { id, items = { {call, res, kind, label} } }
	local function close_group()
		if group then
			U.finish(group.id)
			group = nil
		end
	end

	local function push_done(kind, t)
		close_group()
		U.finish(U.push(kind, t))
	end

	-- "⎿ Interrupted" already shown for the turn the next `<turn_aborted>` marker ends
	local interrupted_shown = false

	local function push_user(text, m, is_replay)
		interrupted_shown = false
		local nodes = {}
		if text ~= "" then
			local rendered = U.transform_markdown(text, {
				message_type = "user",
				is_streaming = false,
				available_width = U.width(),
				created_at_ms = m and m.timestamp,
			})
			nodes[#nodes + 1] = U.UserMessage:render(rendered)
		end
		-- On replay (or when the `images` plugin is not loaded), render attached images through
		-- the same core `mare.ui.image` primitive instead of a hardcoded text stub.
		if is_replay or not ctx:get("images") then
			for _, img in ipairs(user_image_nodes(m)) do
				nodes[#nodes + 1] = img
			end
		end
		if #nodes == 1 then
			push_done("user", { node = nodes[1] })
		elseif #nodes > 1 then
			push_done("user", { node = ui.Lines(nodes) })
		end
	end

	--- "⎿ Interrupted" under what the interrupted turn left (live and replayed alike)
	local function push_interrupted()
		interrupted_shown = true
		push_done("info", {
			node = ui.Line({
				ui.Span("  ⎿  "):hl("MareToolGutter"),
				ui.Span("Interrupted"):hl("MareWarning"),
			}),
		})
	end

	--- `!cmd` as the model sees it (shell-mode's `<bash command="…" exit_code="N">` message,
	--- appended or steered) → command, exit code, output; nil for any other text
	local function parse_shell(text)
		local cmd, code, body =
			string.match(text or "", '^<bash command="(.-)" exit_code="(%-?%d+)">\n?(.-)\n?</bash>%s*$')
		if not cmd then
			return nil
		end
		return (string.gsub(cmd, "&quot;", '"')), tonumber(code), body
	end

	--- a replayed `!cmd`: "! cmd" with its output hanging below, the way shell-mode showed it
	--- live — never the raw tag
	local function push_shell(cmd, code, body)
		local head = ui.Line({
			ui.Span("! "):hl(code == 0 and "MareAccent" or "MareError"),
			ui.Span(cmd):hl("MareToolName"),
		})
		local text = string.gsub(body or "", "%s+$", "")
		if text == "" then
			text = "(no output)"
		end
		local function node(expanded)
			local b = U.ToolCall:result_text(text, code ~= 0, true)
			if not expanded then
				b = ui.Clip(b, cfg.preview_lines)
			end
			return ui.Lines({ head, U.ToolCall:body(b) })
		end
		push_done("tool", { node = node(false), expanded = node(true) })
	end

	--- a user-role message that isn't the user's prompt, shown as a notice: its first line
	--- (a finished background job, …). Model-only markers never show their raw text:
	--- `<turn_aborted>` reads "Interrupted" (unless the answer it follows already does), and
	--- `context_disclosure` messages (`<system-reminder>`, …) stay hidden from the transcript.
	local function push_notification(m)
		if m.context_disclosure then
			return
		end
		if m.turn_aborted then
			if not interrupted_shown then
				push_interrupted()
			end
			interrupted_shown = false
			return
		end
		local first = string.match(user_text(m) or "", "^[^\n]*") or ""
		push_done("info", { node = U.Notice:render(first, "info") })
	end

	local function finish_reasoning()
		if not cur.reasoning then
			return
		end
		local secs = (mare.task.clock() - cur.reasoning_start) / 1000
		local text = U.transform_markdown(cur.reasoning.buffer:snapshot(), {
			message_type = "assistant-reasoning",
			is_streaming = false,
			available_width = U.width(),
			message_id = tostring(cur.reasoning.id),
		})
		U.update(cur.reasoning.id, {
			node = U.Reasoning:render(text, secs),
			expanded = U.Reasoning:expanded(text, secs),
		})
		U.finish(cur.reasoning.id)
		cur.reasoning = nil
	end

	local function render_assistant(text)
		local r = U.get_renderer("message:assistant")
		local ok, node
		if r and r.compact then
			ok, node = pcall(r.compact, text)
		elseif U.AssistantMessage.render then
			ok, node = pcall(U.AssistantMessage.render, U.AssistantMessage, text)
		end
		return ok and node or nil
	end

	local function append_citations(text, citations)
		if type(citations) ~= "table" or #citations == 0 then
			return text
		end
		local seen, lines = {}, {}
		for _, c in ipairs(citations) do
			if type(c) == "table" and type(c.url) == "string" and c.url ~= "" and not seen[c.url] then
				seen[c.url] = true
				local label = (type(c.title) == "string" and c.title ~= "") and c.title or c.url
				lines[#lines + 1] = ("- [%s](%s)"):format(label, c.url)
			end
		end
		if #lines == 0 then
			return text
		end
		local sep = (text == "" or text:sub(-1) == "\n") and "\n" or "\n\n"
		return text .. sep .. "Sources:\n" .. table.concat(lines, "\n")
	end

	local function finish_text()
		if not cur.text then
			return
		end
		local raw = cur.text.citations_text or cur.text.buffer:snapshot()
		local text = U.transform_markdown(raw, {
			message_type = "assistant",
			is_streaming = false,
			available_width = U.width(),
			message_id = tostring(cur.text.id),
		})
		local node = render_assistant(text)
		if node then
			U.update(cur.text.id, { node = node })
		elseif text ~= raw or cur.text.dirty then
			U.update(cur.text.id, { markdown = text })
		end
		present_text(cur.text.id, raw, node, false)
		U.finish(cur.text.id)
		cur.text = nil
	end

	local function working(label, background)
		local tokens = cur.out_chars // 4
		U.working(
			label,
			tokens > 0
					and ("↓ ~%s tokens"):format(
						tokens >= 1000 and ("%.1fk"):format(tokens / 1000) or tostring(tokens)
					)
				or nil,
			background and { background = true } or nil
		)
	end

	--- an error for the screen: no mlua "runtime error: " prefix and no stack traceback (a
	--- dozen lines of plugin internals for an unreachable proxy)
	local function clean_error(e)
		e = tostring(e)
		e = string.match(e, "^(.-)%s*stack traceback:") or e
		while string.find(e, "^runtime error: ") do
			e = string.sub(e, 16)
		end
		return (string.gsub(e, "%s+$", ""))
	end

	local function invoke_custom_renderer(label, compact_fn, expanded_fn, payload, block_kind)
		local ok, node = pcall(compact_fn, payload, { expanded = false })
		if not ok then
			push_done("error", { node = U.Notice:render(label .. ": " .. clean_error(node), "error") })
			return true
		end
		if node == nil then
			return false
		end
		local exp_node
		if expanded_fn then
			local eok, en = pcall(expanded_fn, payload, { expanded = true })
			if eok then
				exp_node = en
			end
		end
		push_done(block_kind, { node = node, expanded = exp_node })
		return true
	end

	local function render_custom_message(m)
		if m.display == false then
			return
		end
		local msg_svc = ctx:get("message")
		local spec = msg_svc and msg_svc:get_type(m.custom_type)
		local compact_fn = spec and (spec.compact or spec.render)
		local expanded_fn = spec and (spec.expanded or spec.render or compact_fn)
		if compact_fn then
			local rendered = invoke_custom_renderer(
				"message renderer error (" .. tostring(m.custom_type) .. ")",
				compact_fn,
				expanded_fn,
				m,
				"custom"
			)
			if rendered then
				return
			end
		end
		local md = U.transform_markdown(user_text(m), {
			message_type = "custom",
			is_streaming = false,
			available_width = U.width(),
			created_at_ms = m.timestamp,
		})
		local head = ui.Line({ ui.Span("[" .. tostring(m.custom_type or "custom") .. "]"):hl("MareAccent") })
		local body = ui.Indent("  ", ui.Markdown(md))
		push_done("custom", { node = ui.Lines({ head, body }) })
	end

	local function render_custom_entry(e)
		if type(e) ~= "table" or type(e.kind) ~= "string" then
			return
		end
		local r = U.get_entry_renderer(e.kind)
		if not r or not (r.compact or r.expanded) then
			return
		end
		invoke_custom_renderer(
			"entry renderer error (" .. tostring(e.kind) .. ")",
			r.compact or r.expanded,
			r.expanded or r.compact,
			e,
			"entry"
		)
	end

	on("turn/started", function(ev)
		turn_documents = {}
		attempt_blocks, attempt_presentations = {}, {}
		interrupted_shown = false
		-- the user's prompt, given back to the input box if it isn't sent
		cur.prompt = type(ev) == "table" and not ev.source and ev.text or nil
		cur.prompt_images = cur.prompt and ev.images or nil
		cur.prompt_image_markers = ev.image_markers
		cur.out_chars = 0
		cur.started = mare.task.clock()
		cur.last_working_label = nil
		working(W.verb("reasoning", "Thinking"))
	end)

	on("step/started", function()
		attempt_blocks, attempt_presentations = {}, {}
	end)

	on("session/recorded", function(ev)
		if type(ev) == "table" and type(ev.entry) == "table" then
			render_custom_entry(ev.entry)
		end
	end)

	on("item/completed", function(ev)
		local item = type(ev) == "table" and ev.item
		if not item then
			return
		end
		if item.type == "tool_call" then
			local cid = item.tool_call_id
			local t = cid and cur.tools[cid]
			if not t then
				return
			end
			cancel_tool_flush(t)
			cur.tools[cid] = nil
			local call = t.call or item
			local msg = item.message
			local res = msg and { content = msg.content, is_error = msg.is_error, details = msg.details } or nil
			if t.item then
				t.item.res = res
				if group and group.id == t.id then
					U.update(group.id, { node = group_node(group, false), expanded = group_node(group, true) })
				end
			elseif call.name == "bash" then
				U.update(t.id, { node = bash_node(call, res, false), expanded = bash_node(call, res, true) })
				U.finish(t.id)
			else
				U.update(t.id, { node = tool_node(call, res, false), expanded = tool_node(call, res, true) })
				U.finish(t.id)
			end
			working(W.verb("reasoning", "Thinking"))
			return
		end
		local m = item.message
		if not m then
			return
		end
		if m.role == "user" and (m.shell or parse_shell(user_text(m))) then
			-- `!cmd` output (appended, or steered into a running turn): shell-mode showed it
		elseif m.role == "user" and (m.notification or m.turn_aborted or m.context_disclosure) then
			push_notification(m)
		elseif m.role == "user" then
			push_user(user_text(m), m)
		elseif m.role == "custom" then
			render_custom_message(m)
		elseif m.role == "assistant" then
			local citations = {}
			for _, b in ipairs(m.content or {}) do
				if b.type == "text" and type(b.citations) == "table" then
					for _, c in ipairs(b.citations) do
						citations[#citations + 1] = c
					end
				end
			end
			if cur.text and #citations > 0 then
				local raw = cur.text.buffer:snapshot()
				local updated = append_citations(raw, citations)
				if updated ~= raw then
					cur.text.citations_text = updated
					cur.text.dirty = true
				end
			end
			finish_reasoning()
			finish_text()
			if m.stop_reason == "error" then
				cur.error_shown = true
				push_done("error", { node = U.Notice:render(clean_error(m.error or "error"), "error") })
			elseif m.stop_reason == "aborted" then
				push_interrupted()
			end
		end
	end)

	-- item/delta arrives in coalesced chunks (the agent coalesces deltas), not per token
	on("item/delta", function(ev)
		if type(ev) ~= "table" then
			return
		end
		if ev.kind == "tool_output" then
			local cid = ev.tool_call_id or ev.item_id
			local t = cid and cur.tools[cid]
			if not t or t.done then
				return
			end
			if ev.partial and ev.partial.output then
				if t.item then
					return -- exploration calls show no live output
				end
				t.pending_partial = ev.partial.output
			elseif type(ev.delta) == "string" and ev.delta ~= "" then
				if t.item then
					return -- exploration calls show no live output
				end
				if not t.output_buf then
					t.output_buf = mare.stream.buffer()
				end
				t.output_buf:append(ev.delta)
				t.pending_partial = nil
			else
				return
			end
			local now = mare.task.clock()
			local interval = cfg.tool_update_interval_ms or 100
			local elapsed = now - t.last
			if elapsed >= interval then
				t.pending_flush = false
				flush_tool_output(t)
				return
			end
			-- Leading + trailing edge throttle: a burst followed by silence still displays the
			-- final lines once the remaining throttle window elapses.
			t.pending_flush = true
			if not t.flush_scheduled then
				t.flush_scheduled = true
				local gen = (t.flush_gen or 0) + 1
				t.flush_gen = gen
				local wait = math.max(1, interval - elapsed)
				ctx:spawn(function()
					mare.task.sleep(wait)
					t.flush_scheduled = false
					if t.flush_gen == gen and t.pending_flush and not t.done then
						t.pending_flush = false
						flush_tool_output(t)
					end
				end)
			end
			return
		end
		local inner_ev = ev.kind == "event" and ev.event or nil
		local ev_type = inner_ev and inner_ev.type
		if ev.kind == "text" then
			finish_reasoning()
			if not cur.text then
				close_group()
				local bid = U.push("assistant", { markdown = "" })
				attempt_blocks[#attempt_blocks + 1] = bid
				cur.text = { id = bid, buffer = mare.stream.buffer() }
			end
			cur.text.buffer:append(ev.delta)
			if U.has_markdown_transforms("assistant") then
				local md = U.transform_markdown(cur.text.buffer:snapshot(), {
					message_type = "assistant",
					is_streaming = true,
					available_width = U.width(),
					message_id = tostring(cur.text.id),
				})
				U.update(cur.text.id, {
					markdown = md,
				})
				present_text(cur.text.id, md, nil, false)
			else
				U.append(cur.text.id, ev.delta)
				local view = presentation_views[tostring(cur.text.id)]
				if (D.has_markdown_presenters and D.has_markdown_presenters()) or (view and view.projected) then
					present_text(cur.text.id, cur.text.buffer:snapshot(), nil, false)
				end
			end
			cur.out_chars = cur.out_chars + #ev.delta
		elseif ev.kind == "reasoning" then
			if not cur.reasoning then
				close_group()
				cur.reasoning_start = mare.task.clock()
				local bid = U.push("reasoning", { markdown = "" })
				attempt_blocks[#attempt_blocks + 1] = bid
				cur.reasoning = { id = bid, buffer = mare.stream.buffer() }
			end
			cur.reasoning.buffer:append(ev.delta)
			if U.has_markdown_transforms("assistant-reasoning") then
				U.update(cur.reasoning.id, {
					markdown = U.transform_markdown(cur.reasoning.buffer:snapshot(), {
						message_type = "assistant-reasoning",
						is_streaming = true,
						available_width = U.width(),
						message_id = tostring(cur.reasoning.id),
					}),
				})
			else
				U.append(cur.reasoning.id, ev.delta)
			end
			cur.out_chars = cur.out_chars + #ev.delta
		elseif ev.kind == "tool_arguments" then
			cur.out_chars = cur.out_chars + #(ev.delta or "")
		elseif ev_type == "tool_call_start" then
			finish_reasoning()
			finish_text()
		elseif ev_type == "hosted_tool_start" or ev_type == "hosted_tool_update" then
			working(W.verb("exploring", "Searching"))
		end
		local now = mare.task.clock()
		local label = cur.reasoning and W.verb("reasoning", "Thinking") or W.verb("responding", "Responding")
		if label ~= cur.last_working_label or now - cur.last_working > W.text_throttle_ms() then
			cur.last_working = now
			cur.last_working_label = label
			working(label)
		end
	end)

	on("ui/presenter-changed", function(ev)
		if ev and ev.action == "registered" and cur.text then
			local source = cur.text.buffer:snapshot()
			if U.has_markdown_transforms("assistant") then
				source = U.transform_markdown(source, {
					message_type = "assistant",
					is_streaming = true,
					available_width = U.width(),
					message_id = tostring(cur.text.id),
				})
			end
			present_text(cur.text.id, source, nil, false)
		end
	end)

	on("pipeline/tool-dispatching", function(ev)
		local call = ev.call
		local kind, label = classify(call)
		if kind ~= "Unknown" then
			-- joins (or starts) the exploration group; no separate block
			local item = { call = call, kind = kind, label = label }
			if not group then
				group = { id = U.push("tool", {}), items = {} }
			end
			table.insert(group.items, item)
			U.update(group.id, { node = group_node(group, false) })
			cur.tools[call.id] = { id = group.id, call = call, last = 0, item = item }
			working(W.verb("exploring", "Exploring"))
			return
		end
		close_group()
		local node = call.name == "bash" and bash_node(call, nil, false) or tool_node(call, nil, false)
		local id = U.push("tool", { node = node })
		cur.tools[call.id] = { id = id, call = call, last = 0 }
		-- the tool's verb in the present tense when it has one
		-- shell commands carry `background`: the core offers the background key under them,
		-- whatever `verbs.shell` words the label
		local display_call = unwrap_call_tool(call)
		local display_name = display_call and display_call.name or call.name
		local shell = display_name == "bash" or display_name == "powershell"
		working(
			W.verb(
				shell and "shell" or "tool",
				U.ToolCall:verb(display_name, "running") or ("Running " .. display_name)
			),
			shell
		)
	end)

	on("turn/completed", function(res)
		finish_reasoning()
		finish_text()
		if res and (res.error or res.status == "cancelled" or res.termination_reason == "interrupted") then
			for id in pairs(turn_documents) do
				presentation_service.cancel_markdown(presentation_service, ctx, id)
			end
		end
		close_group()
		U.working(nil)
		for _, t in pairs(cur.tools) do
			cancel_tool_flush(t)
			U.finish(t.id)
		end
		cur.tools = {}
		local interrupted = res and (res.status == "cancelled" or res.termination_reason == "interrupted")
		local failed = res and res.error and not interrupted
		if failed then
			if not cur.error_shown then
				local text = clean_error(require("mare_lib").error_message(res.error))
				push_done("error", { node = U.Notice:render(res.not_sent and ("not sent: " .. text) or text, "error") })
			end
		end
		cur.error_shown = false
		-- a dim closing line per turn: "worked 4s · 1.2k tokens · 02:46", or "interrupted after 4s"
		-- nothing was sent: no "stopped after 0s" line under the "not sent" notice
		if res and res.not_sent then
			cur.started = nil
			-- "not sent: <reason>" above, the prompt back in the input box, in front of
			-- anything typed since
			if cur.prompt and (cur.prompt ~= "" or (cur.prompt_images and #cur.prompt_images > 0)) then
				U.set_editor(
					cur.prompt,
					{ prepend = true, images = cur.prompt_images, image_markers = cur.prompt_image_markers }
				)
			end
		end
		cur.prompt = nil
		cur.prompt_images = nil
		if cur.started then
			local secs = (mare.task.clock() - cur.started) / 1000
			cur.started = nil
			local node = U.TurnEnd:render({
				secs = secs,
				interrupted = interrupted,
				failed = failed,
				output_tokens = res and res.usage and res.usage.output or 0,
				clock = os.date("%H:%M"),
			})
			if node then
				push_done("info", { node = node })
			end
		end
	end)

	on("step/completed", function(ev)
		if not (type(ev) == "table" and ev.will_retry) then
			return
		end
		if ev.reset then
			-- the partial answer is discarded and the request re-sent: drop what was streamed
			for key in pairs(attempt_presentations) do
				pcall(presentation_service.delete_markdown, presentation_service, ctx, key)
				presentation_views[key] = nil
				turn_documents[key] = nil
			end
			for i = #attempt_blocks, 1, -1 do
				pcall(U.remove, attempt_blocks[i])
			end
			attempt_blocks, attempt_presentations = {}, {}
			if cur.text then
				local key = tostring(cur.text.id)
				pcall(presentation_service.delete_markdown, presentation_service, ctx, key)
				presentation_views[key] = nil
				turn_documents[key] = nil
				pcall(U.remove, cur.text.id)
				cur.text = nil
			end
			if cur.reasoning then
				pcall(U.remove, cur.reasoning.id)
				cur.reasoning = nil
			end
		end
		local err_msg = (type(ev.error) == "table" and ev.error.message) or ev.error or ""
		U.notify(
			("Retrying (%d) in %.0fs: %s"):format(ev.attempt or 1, (ev.delay_ms or 0) / 1000, tostring(err_msg)),
			"warning"
		)
	end)
	on("host/notice", function(ev)
		U.notify(ev.text or tostring(ev), ev.level or "warning")
	end)
	-- a failing handler: under the turn's prompt during a turn, a notice outside one
	-- (plugins/lua/ui/handler_errors.lua); the traceback is in /plugins, doctor and the log
	local handler_errors
	local function errors()
		handler_errors = handler_errors
			or require("ui.handler_errors")({ U = U, push_done = push_done, clean_error = clean_error })
		return handler_errors
	end
	on("kernel/error", function(ev)
		errors().on_error(ev)
	end)
	-- a plugin stuck loading, unloading or waiting (announced once per stall); one notice per plugin
	on("kernel/stalled", function(ev)
		ui.notify(ev.summary, "warning", { id = "plugin-stalled:" .. ev.plugin })
	end)
	on("turn/started", function()
		errors().turn_start()
	end)
	on("turn/completed", function()
		errors().turn_end()
	end)

	-- replay history after resume / tree navigation
	on("session/loaded", function(ev)
		presentation_views, turn_documents = {}, {}
		U.clear()
		group = nil -- its block went with the clear
		interrupted_shown = false
		local title = ev.name and ("Resumed: " .. ev.name) or (ev.navigated and "Moved to branch" or "Resumed session")
		push_done("info", { node = ui.Line({ ui.Span("⎯ " .. title .. " ⎯"):hl("MareDim") }) })
		local pending = {}
		-- render only the most recent messages; start at a user message so
		-- tool results never lose their calls; older ones stay in the session / Ctrl+T
		local all_msgs = ev.messages or {}
		local first = 1
		local limit = 30
		if #all_msgs > limit then
			first = #all_msgs - limit + 1
			while first > 1 and all_msgs[first].role ~= "user" do
				first = first - 1
			end
			push_done("info", {
				node = ui.Line({
					ui.Span(("… %d earlier messages (/tree to browse)"):format(first - 1)):hl("MareDim"),
				}),
			})
		end
		-- collect durable ui/card records and align them with message/compaction entries
		local cards_after = {}
		local tail_cards = {}
		local anchor_ids = {}
		local session_service = ctx:get("session")
		if session_service and session_service.list_records then
			local cards = require("mare_lib").all_records(session_service, "ui/card", { loaded_only = true }) or {}
			if #cards > 0 then
				local entries = (session_service.entries and session_service:entries({ expand = false })) or {}
				local start_idx = 1
				for i, e in ipairs(entries) do
					if e.type == "compaction" then
						start_idx = i
					end
				end
				local entry_anchor = {}
				local last_anchor
				for i = start_idx, #entries do
					local e = entries[i]
					if e.type == "message" or e.type == "compaction" then
						last_anchor = e.entry_id
						anchor_ids[#anchor_ids + 1] = e.entry_id
					end
					if e.entry_id and last_anchor then
						entry_anchor[e.entry_id] = last_anchor
					end
				end
				local aligned = (#anchor_ids == #all_msgs)
				for _, card in ipairs(cards) do
					local aid = aligned and card.after_entry_id and entry_anchor[card.after_entry_id] or nil
					if aid then
						cards_after[aid] = cards_after[aid] or {}
						cards_after[aid][#cards_after[aid] + 1] = card
					else
						tail_cards[#tail_cards + 1] = card
					end
				end
			end
		end
		local function emit_cards(list)
			if not list then
				return
			end
			close_group()
			for _, card in ipairs(list) do
				push_done(card.kind or "info", {
					title = card.title or "",
					markdown = card.markdown or "",
				})
			end
		end
		local custom_entries, ce_idx = {}, 1
		if session_service and session_service.entries then
			local min_ts = first > 1 and all_msgs[first] and all_msgs[first].timestamp or nil
			for _, e in ipairs(session_service:entries({ expand = false }) or {}) do
				if e.type == "custom" and U.get_entry_renderer(e.kind) then
					if not min_ts or not e.created_at_ms or e.created_at_ms >= min_ts then
						custom_entries[#custom_entries + 1] = e
					end
				end
			end
		end
		local function flush_custom_until(ts)
			while ce_idx <= #custom_entries do
				local ce = custom_entries[ce_idx]
				if ts and ce.created_at_ms and ce.created_at_ms > ts then
					break
				end
				render_custom_entry(ce)
				ce_idx = ce_idx + 1
			end
		end
		for idx, m in ipairs(all_msgs) do
			if idx < first then
				goto continue
			end
			flush_custom_until(m.timestamp)
			if m.role == "user" then
				if m.compaction then
					local summary_md = U.transform_markdown(m.summary or user_text(m), {
						message_type = "summary",
						is_streaming = false,
						available_width = U.width(),
						created_at_ms = m.timestamp,
					})
					push_done("info", {
						node = ui.Line({
							ui.Span("◆ "):hl("MareMark"),
							ui.Span("Conversation compacted"):hl("MareDim"),
						}),
						expanded = ui.Markdown(summary_md),
					})
				elseif m.notification or m.turn_aborted or m.context_disclosure then
					push_notification(m)
				else
					local cmd, code, body = parse_shell(user_text(m))
					if cmd then
						push_shell(cmd, code, body)
					else
						push_user(user_text(m), m, true)
					end
				end
			elseif m.role == "custom" then
				render_custom_message(m)
			elseif m.role == "assistant" then
				for _, b in ipairs(m.content or {}) do
					if b.type == "text" and b.text ~= "" then
						local md = U.transform_markdown(append_citations(b.text, b.citations), {
							message_type = "assistant",
							is_streaming = false,
							available_width = U.width(),
							created_at_ms = m.timestamp,
						})
						local node = render_assistant(md)
						close_group()
						local id = U.push("assistant", node and { node = node } or { markdown = md })
						present_text(id, md, node, true)
						U.finish(id)
					elseif b.type == "tool_call" and not b.hosted then
						pending[b.id] = b
					end
				end
				if m.stop_reason == "aborted" then
					push_interrupted()
				end
			elseif m.role == "tool_result" then
				local call = pending[m.tool_call_id] or { name = m.tool_name, arguments = {} }
				local res = { content = m.content, is_error = m.is_error, details = m.details }
				-- the same blocks the live view drew: exploration groups, "Ran cmd", tool cards
				local kind, label = classify(call)
				if kind ~= "Unknown" then
					if not group then
						group = { id = U.push("tool", {}), items = {} }
					end
					table.insert(group.items, { call = call, kind = kind, label = label, res = res })
					U.update(group.id, { node = group_node(group, false), expanded = group_node(group, true) })
				elseif call.name == "bash" then
					push_done("tool", { node = bash_node(call, res, false), expanded = bash_node(call, res, true) })
				else
					push_done("tool", { node = tool_node(call, res, false), expanded = tool_node(call, res, true) })
				end
			end
			if anchor_ids[idx] then
				emit_cards(cards_after[anchor_ids[idx]])
			end
			::continue::
		end
		flush_custom_until(nil)
		close_group()
		emit_cards(tail_cards)
	end)

	on("session/new", function()
		presentation_views, turn_documents = {}, {}
		U.clear()
		group = nil -- its block went with the clear
		interrupted_shown = false
		local banner = D.new_session_banner and D.new_session_banner()
		push_done("info", {
			node = banner or ui.Line({ ui.Span("⎯ New session ⎯"):hl("MareDim") }),
		})
	end)

	return { push_done = push_done, dispatch = dispatch }
end
