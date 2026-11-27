-- /tree: the session tree view, for the command registered by plugins/session.lua (the
-- entry layer); loaded on the first /tree. `D` carries the session state and
-- helpers it uses (shared with the entry, never copies).
return function(D)
	local ctx = D.ctx
	local S = D.S
	local cfg = D.cfg
	local session = D.session
	local path_to = D.path_to
	local abandoned_messages = D.abandoned_messages
	local refuse_busy = D.refuse_busy
	local build_branch_summary = D.build_branch_summary
	local move_leaf = D.move_leaf
	local ensure_full = D.ensure_full
	local when = D.when
	local own_prompt = D.own_prompt
	local user_text = D.user_text
	local add = D.add
	local index_update = D.index_update
	local tool_names = D.tool_names

	local TREE_FILTERS = { "default", "no-tools", "user", "labeled", "all" }
	local FILTER_TITLE = {
		default = "default",
		["no-tools"] = "no tools",
		user = "your messages",
		labeled = "labeled",
		all = "everything",
	}
	local function shown(e, filter)
		local m = e.type == "message" and e.message
		if filter == "labeled" then
			return S.labels[e.entry_id] ~= nil
		end
		if filter == "all" then
			return true
		end
		if m then
			if m.role == "user" then
				return own_prompt(m)
			elseif filter == "user" or m.role == "tool_result" then
				return false
			elseif m.role == "assistant" then
				return not (filter == "no-tools" and user_text(m) == "")
			elseif m.role == "custom" then
				return filter ~= "user" and m.display ~= false
			end
			return false
		end
		return filter ~= "user"
	end
	local function node_text(e)
		local m = e.type == "message" and e.message
		local one = function(t)
			return (string.gsub(t or "", "%s+", " "))
		end
		if e.type == "compaction" then
			return "▣ compacted" .. (e.tokens_before and (" (" .. e.tokens_before .. " tokens before)") or "")
		elseif e.type == "branch_summary" then
			return "◇ branch summary: " .. one(e.summary)
		elseif not m then
			return e.type
		elseif m.role == "user" and m.turn_aborted then
			-- the model-only interrupt marker: never its raw `<turn_aborted>` text
			return "· interrupted"
		elseif m.role == "user" and m.context_disclosure then
			return "· context ("
				.. tostring(m.context_disclosure.namespace or m.context_disclosure.source or "disclosure")
				.. ")"
		elseif m.role == "user" then
			return (own_prompt(m) and "› " or "· ") .. one(user_text(m))
		elseif m.role == "assistant" then
			local t = one(user_text(m))
			local tools = tool_names(m)
			if #tools > 0 then
				t = (t ~= "" and (t .. "  ") or "") .. "⚙ " .. table.concat(tools, ", ")
			end
			return t ~= "" and t or "(empty answer)"
		elseif m.role == "tool_result" then
			return "  ↳ " .. (m.is_error and "error: " or "") .. one(user_text(m))
		elseif m.role == "custom" then
			return ("[%s] %s"):format(tostring(m.custom_type or "custom"), one(user_text(m)))
		end
		return one(user_text(m))
	end

	--- rows of the visible tree: a hidden node's children hang from its nearest shown ancestor
	local function tree_rows(filter, folded)
		local nodes, vis = {}, {}
		for _, e in ipairs(S.list) do
			if
				e.entry_id
				and S.entries[e.entry_id] == e
				and (e.type == "message" or e.type == "compaction" or e.type == "branch_summary")
			then
				nodes[#nodes + 1] = e
				if shown(e, filter) then
					vis[e.entry_id] = true
				end
			end
		end
		local vp_cache = {}
		local function vparent(e)
			local chain, p = {}, e.parent_entry_id
			while p and not vis[p] and vp_cache[p] == nil do
				chain[#chain + 1] = p
				local pe = S.entries[p]
				p = pe and pe.parent_entry_id
				if #chain > #nodes then
					p = nil -- a parent cycle: treat as a root
				end
			end
			local r
			if p == nil then
				r = false
			elseif vis[p] then
				r = p
			else
				r = vp_cache[p]
			end
			for _, c in ipairs(chain) do
				vp_cache[c] = r
			end
			return r or nil
		end
		local ROOT = {}
		local kids = { [ROOT] = {} }
		for _, e in ipairs(nodes) do
			if vis[e.entry_id] then
				local p = vparent(e) or ROOT
				kids[p] = kids[p] or {}
				table.insert(kids[p], e)
			end
		end
		local rows, stack = {}, {}
		local function push(list, prefix)
			local branching = #list > 1
			for i = #list, 1, -1 do
				local last = i == #list
				stack[#stack + 1] = {
					e = list[i],
					head = prefix .. (branching and (last and "└─ " or "├─ ") or ""),
					rest = prefix .. (branching and (last and "   " or "│  ") or ""),
				}
			end
		end
		local function descendants(id)
			local n, st = 0, { id }
			while #st > 0 do
				local x = table.remove(st)
				for _, k in ipairs(kids[x] or {}) do
					n = n + 1
					st[#st + 1] = k.entry_id
				end
			end
			return n
		end
		push(kids[ROOT], "")
		while #stack > 0 do
			local it = table.remove(stack)
			local e = it.e
			local row = { e = e, prefix = it.head, children = #(kids[e.entry_id] or {}) }
			rows[#rows + 1] = row
			if folded[e.entry_id] and row.children > 0 then
				row.hidden = descendants(e.entry_id)
			else
				push(kids[e.entry_id] or {}, it.rest)
			end
		end
		return rows, vis
	end

	--- leave the current branch for `target`: ask about a summary of what's left behind
	--- (no / yes / with your own instructions), move, then summarize — Esc stops it.
	--- Returns false when the user backed out.
	local function navigate(target, editor_text)
		local abandoned = abandoned_messages(target)
		local summarize, instructions = false, nil
		if #abandoned > 0 and cfg.branch_summary ~= false and D.follow.llm() then
			local c = ui.select({
				title = ("Leaving a branch of %d message%s — summarize it for the new one?"):format(
					#abandoned,
					#abandoned == 1 and "" or "s"
				),
				items = {
					{ label = "No summary" },
					{ label = "Summarize", detail = "what was tried, learned and failed" },
					{ label = "Summarize with my own instructions" },
				},
			})
			if not c then
				ui.notify("Navigation cancelled")
				return false
			end
			summarize = c >= 2
			if c == 3 then
				instructions = ui.input({ title = "Instructions for the branch summary" })
				if not instructions then
					ui.notify("Navigation cancelled")
					return false
				end
			end
		end
		if refuse_busy("/tree") then
			return false
		end
		local from_leaf = S.leaf
		local owner = S.id
		if summarize then
			if target ~= nil and not S.entries[target] then
				ui.notify("Not moved: no such session entry: " .. tostring(target), "warning")
				return false
			end
			local before =
				ctx.policy:decide("session/before-tree", table.pack({ target_id = target, current_leaf = S.leaf }))
			if not before or before.decision ~= "allow" then
				ui.notify(
					"Not moved: " .. tostring((before and before.reason) or "tree navigation cancelled"),
					"warning"
				)
				return false
			end
			local cancelled, done, summary_entry = false, false, nil
			local off = ctx:on("key/interrupt", function()
				cancelled = true
			end)
			ctx:spawn(function()
				local ok_s, res = pcall(build_branch_summary, abandoned, from_leaf, target, {
					instructions = instructions,
					cancelled = function()
						return cancelled
					end,
				})
				if ok_s and type(res) == "table" then
					summary_entry = res
				end
				done = true
			end)
			while not done and not cancelled do
				mare.task.sleep(30)
			end
			off()
			if S.id ~= owner then
				return false
			end
			if cancelled and not done then
				mare.ui.working(nil)
				ui.notify("Branch summary cancelled (Esc) — moved without one", "info")
				summary_entry = nil
			end
			local ok, err = move_leaf(ctx, target, { summary_entry = summary_entry, before_checked = true })
			if not ok then
				ui.notify("Not moved: " .. tostring(err), "warning")
				return false
			end
		else
			local ok, err = session.set_leaf(session, ctx, target)
			if not ok then
				ui.notify("Not moved: " .. tostring(err), "warning")
				return false
			end
		end
		if editor_text then
			ui.set_editor(editor_text, { keep_draft = true })
		end
		return true
	end

	if refuse_busy("/tree") then
		return
	end
	ensure_full()
	local st = { filter = 1, folded = {}, filter_text = "", sel_id = S.leaf }
	while true do
		local fname = TREE_FILTERS[st.filter]
		local rows, vis = tree_rows(fname, st.folded)
		if #rows == 0 and fname == "default" then
			ui.notify("Nothing to navigate yet")
			return
		end
		local on_path = {}
		for _, e in ipairs(path_to(S.leaf)) do
			on_path[e.entry_id] = true
		end
		-- the leaf, or its nearest shown ancestor, starts highlighted
		local want = st.sel_id
		while want and not vis[want] do
			local we = S.entries[want]
			want = we and we.parent_entry_id
		end
		local items, sel = {}, #rows
		for i, r in ipairs(rows) do
			local e = r.e
			local mark = e.entry_id == S.leaf and "◉ " or (on_path[e.entry_id] and "● " or "○ ")
			local lab = S.labels[e.entry_id] and ("  [" .. S.labels[e.entry_id] .. "]") or ""
			local fold = r.hidden and ("  ▸ +" .. r.hidden) or ""
			items[i] = {
				label = r.prefix .. mark .. mare.text.trunc_width(node_text(e), 70) .. lab .. fold,
				detail = (e.entry_id == S.leaf and "here · " or "") .. when(e),
			}
			if e.entry_id == want then
				sel = i
			end
		end
		if #items == 0 then
			items[1] = { label = "(nothing matches this filter)", header = true }
		end
		local i, _, key, filter = ui.select({
			-- Enter goes there; a message of yours also goes to the input
			title = ("Session tree · %s"):format(FILTER_TITLE[fname]),
			items = items,
			selected = sel,
			filter = st.filter_text,
			keys = {
				{
					key = "ctrl+f",
					label = "filter",
				},
				{ key = "ctrl+e", label = "fold" },
				{ key = "ctrl+t", label = "label" },
				{ key = "ctrl+y", label = "copy" },
			},
		})
		if not i or not rows[i] then
			return
		end
		st.filter_text = filter or ""
		local e = rows[i].e
		st.sel_id = e.entry_id
		if key == "ctrl+f" then
			st.filter = st.filter % #TREE_FILTERS + 1
		elseif key == "ctrl+e" then
			st.folded[e.entry_id] = not st.folded[e.entry_id] or nil
		elseif key == "ctrl+t" then
			local text = ui.input({
				title = "Label for this point (empty clears)",
				placeholder = S.labels[e.entry_id] or "",
			})
			if text then
				local ok, err = add({
					type = "label",
					target_entry_id = e.entry_id,
					label = text,
					created_at_ms = require("mare_lib").to_epoch_ms(mare.task.now()),
				})
				if ok then
					S.labels[e.entry_id] = text ~= "" and text or nil
					index_update()
				else
					ui.notify("Label not saved: " .. tostring(err), "warning")
				end
			end
		elseif key == "ctrl+y" then
			-- `y`: copy that message instead of branching (the copy plugin,
			-- :1226)
			local m = e.message or {}
			local text = e.summary or user_text(m)
			local copy = ctx:get("copy")
			if copy and copy.text and type(text) == "string" and text ~= "" then
				local kind = m.role or e.type
				local what = kind == "user" and "the message"
					or (kind and ("the " .. kind .. " message") or "the message")
				ctx:spawn(function()
					pcall(copy.text, copy, text, what)
				end)
			end
			return
		else
			if refuse_busy("/tree") then
				return
			end
			local m = e.type == "message" and e.message
			local user = m and own_prompt(m)
			-- your message: go to just before it, its text to the input; any
			-- other node: go to it, the input stays as it is
			local target = user and e.parent_entry_id or e.entry_id
			if target == S.leaf then
				if user then
					ui.set_editor(user_text(m), { keep_draft = true })
				else
					ui.notify("Already here")
				end
				return
			end
			navigate(target, user and user_text(m) or nil)
			return
		end
	end
end
