-- ui.queue_edit: the messages queued while a turn runs: listed above the input,
-- ↑ selects one (↑/↓ move, Delete removes, Esc leaves), Enter loads the selected one into the
-- input to edit it in place: Enter saves it back at the same position, Esc puts it back
-- unchanged, an emptied input removes it. Which key becomes which `input/queue-*` event is
-- decided by `mare-tui/src/queue_keys.rs`; this holds the state and the list. Constructed once
-- by the ui plugin.
return function(deps)
	local ctx, U, service = deps.ctx, deps.U, deps.service
	local M = {}
	local MODE_OFF, MODE_SELECTING, MODE_EDITING = 0, 1, 2
	local selected -- the entry table under the selection
	local editing -- the entry table whose text is in the input

	--- Detached entries in display order; stable IDs survive edits and snapshot refreshes.
	local function items()
		local agent_service = ctx:get("agent")
		local list = agent_service and (agent_service:list_queue().data or {}) or {}
		local out = {}
		for _, e in ipairs(list) do
			local kind = e.lane == "steer" and "steer" or "follow"
			out[#out + 1] = { kind = kind, entry = e }
		end
		return out
	end
	M.items = items

	local function index_of(list, entry)
		for i, it in ipairs(list) do
			if it.entry.input_id == entry.input_id then
				return i
			end
		end
	end

	local function change(id, text, images, image_markers)
		local agent_service = ctx:get("agent")
		local ok, result = pcall(function()
			if text == nil then
				agent_service:delete_queued(id)
				return true
			end
			return agent_service:update_queued(id, { input = text, images = images, image_markers = image_markers })
		end)
		if not ok then
			U.notify("Queued input was not changed: " .. tostring(result), "error")
		end
		return ok, result
	end

	local function set_mode()
		U.set_queue_mode(editing and MODE_EDITING or (selected and MODE_SELECTING or MODE_OFF))
	end

	service.widget(service, ctx, "queued-messages", {
		slot = "above-editor",
		order = -10,
		render = function(width)
			local list = items()
			if #list == 0 then
				return nil
			end
			local ascii = U.icon_set and U.icon_set() == "ascii"
			local lines = {}
			for _, it in ipairs(list) do
				local text = string.gsub(it.entry.text or "", "%s+", " ")
				local mark, note = ascii and "  > " or "  ↳ ", it.kind == "steer" and "  (steering)" or "  (queued)"
				if selected and it.entry.input_id == selected.input_id then
					mark = ascii and "  * " or "  ▸ "
				elseif editing and it.entry.input_id == editing.input_id then
					mark, note = ascii and "  ~ " or "  ✎ ", "  (editing)"
				end
				local active = (selected and it.entry.input_id == selected.input_id)
					or (editing and it.entry.input_id == editing.input_id)
				local body = ui.Span(mare.text.trunc_width(text, math.max(10, width - 24)))
				lines[#lines + 1] = ui.Line({
					ui.Span(mark):hl(active and "MareAccent" or "MareDim"),
					active and body or body:hl("MareMuted"),
					ui.Span(note):hl("MareDim"),
				})
			end
			return ui.Lines(lines)
		end,
	})

	--- redraw the list and the counter, and drop a selection or an edit whose message the agent
	--- has taken meanwhile (an edit in the input then turns into a plain draft)
	function M.refresh()
		local list = items()
		if selected then
			local at = index_of(list, selected)
			selected = at and list[at].entry or nil
		end
		if editing and not index_of(list, editing) then
			editing = nil
		end
		set_mode()
		service.invalidate(service, ctx, "queued-messages")
		U.set_queued(#list)
	end

	--- ↑ on an empty input: select the newest queued message
	function M.select()
		local list = items()
		selected = list[#list] and list[#list].entry or nil
		M.refresh()
	end

	--- ↑ / ↓ in the selection; ↓ past the newest leaves it
	function M.move(delta)
		local list = items()
		local at = selected and index_of(list, selected)
		if not at then
			return M.refresh()
		end
		local to = at + delta
		selected = to > #list and nil or list[math.max(1, to)].entry
		M.refresh()
	end

	--- Enter on the selection: its text goes into the input
	function M.edit()
		if not selected then
			return M.refresh()
		end
		editing, selected = selected, nil
		U.set_editor(editing.text or "", { images = editing.images, image_markers = editing.image_markers })
		M.refresh()
	end

	--- Esc / Ctrl+C / external editor replacement: leave the selection or cancel editing.
	--- With `opts.keep_editor == true`, leave the current editor buffer intact.
	function M.cancel(opts)
		local keep_editor = type(opts) == "table" and opts.keep_editor == true
		if editing then
			editing = nil
			if not keep_editor then
				U.set_editor("")
			end
		end
		selected = nil
		M.refresh()
	end

	--- Delete on the selection (the selection moves on), or Enter on an emptied input while editing
	function M.remove()
		local was_selected = selected ~= nil
		local target = selected or editing
		local list = items()
		local at = target and index_of(list, target)
		if not at then
			editing = nil
			return M.refresh()
		end
		if not change(target.input_id) then
			return M.refresh()
		end
		if was_selected then
			local next_item = list[at + 1] or list[at - 1]
			selected = next_item and next_item.entry or nil
		else
			editing = nil
			U.set_editor("")
		end
		M.refresh()
	end

	--- the input was submitted: when it holds an edited queued message, the text replaces it in
	--- place (its images stay, new ones join) and the submit is done. Returns true when consumed;
	--- a message the agent took meanwhile is no edit any more, it goes on as a normal submit
	function M.submit(ev)
		if not editing then
			return false
		end
		local entry = editing
		editing = nil
		if not index_of(items(), entry) then
			M.refresh()
			return false
		end
		local ok, changed = change(entry.input_id, ev.text, ev.images, ev.image_markers)
		if not ok then
			editing = entry
			U.set_editor(ev.text, { prepend = true, images = ev.images, image_markers = ev.image_markers })
		elseif not changed then
			M.refresh()
			return false
		end
		M.refresh()
		return true
	end

	return M
end
