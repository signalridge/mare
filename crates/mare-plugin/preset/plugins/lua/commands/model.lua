-- /model: implementation of the command registered by plugins/commands.lua (the entry
-- layer); loaded on the first /model. Runs in the commands plugin's env.
return function(ctx, args)
	local llm = ctx:get("llm")
	local filter = nil
	if args ~= "" then
		-- Resolve through the single shared llm setter: exact ref, alias, or unambiguous bare id
		-- switches directly; ambiguous bare ids, explicit `provider/id` typos, and refusals fail
		-- immediately; other partial strings open the picker pre-filtered.
		local target = llm.alias_target and llm:alias_target(args)
		local ok, res = pcall(llm.set_model, llm, args)
		if ok and res then
			if target and target ~= args then
				ui.notify(("Model: %s (alias %s)"):format(res.ref, args))
			else
				ui.notify("Model: " .. res.ref)
			end
			return
		end
		local err = type(res) == "table" and res.message or tostring(res)
		if
			err:find("ambiguous model", 1, true)
			or args:find("/", 1, true)
			or not err:find("unknown model", 1, true)
		then
			ui.notify(err, "error")
			return false, err
		end
		filter = args
	end
	-- opening /model refreshes stale lists of the providers with credentials, in the
	-- background; the picker shows what is known now and updates in place on refresh
	if llm.refresh_catalog then
		ctx:spawn(function()
			llm:refresh_catalog({ configured = true, on_open = true })
		end)
	end
	-- only providers with credentials are offered; the rest are one group marked
	-- "needs login"
	local function build_picker_items(cur)
		local list = llm:models()
		local usable = {}
		local function can_use(prov)
			if usable[prov] == nil then
				local p = llm:provider(prov)
				usable[prov] = not p or p.no_key == true or not p.is_configured or p.is_configured() == true
			end
			return usable[prov]
		end
		-- decided before sorting: the check may yield (a comparator can't)
		for _, m in ipairs(list) do
			can_use(m.provider)
		end
		table.sort(list, function(a, b)
			local ua, ub = usable[a.provider], usable[b.provider]
			if ua ~= ub then
				return ua
			end
			if a.provider ~= b.provider then
				return a.provider < b.provider
			end
			return a.id < b.id
		end)
		local items, sel = {}, 1
		for i, m in ipairs(list) do
			local parts = { m.name or m.id }
			if m.context_window then
				parts[#parts + 1] = ("%dk context"):format(m.context_window // 1000)
			end
			if m.reasoning and m.efforts then
				parts[#parts + 1] = table.concat(m.efforts, "/")
			end
			local is_cur = cur and m.ref == cur.ref
			-- preview pane
			local function meta(k, v)
				return ui.Line({ ui.Span(("%-10s"):format(k)):hl("MareDim"), ui.Span(tostring(v)) })
			end
			local pv = {
				ui.Line({ ui.Span(m.name or m.id):hl("MareTitle") }),
				ui.Gap(1),
				meta("provider", m.provider or "?"),
				meta("id", m.id or "?"),
			}
			if m.context_window then
				pv[#pv + 1] = meta("context", ("%dk tokens"):format(m.context_window // 1000))
			end
			if m.max_output then
				pv[#pv + 1] = meta("output", ("%dk tokens"):format(m.max_output // 1000))
			end
			pv[#pv + 1] = meta("reasoning", m.reasoning and table.concat(m.efforts or { "yes" }, "/") or "no")
			if m.input then
				pv[#pv + 1] = meta("input", type(m.input) == "table" and table.concat(m.input, ", ") or m.input)
			end
			-- the current model's effective parameters: value ← source layer, the exact wire fields
			-- and the options (* increases cost) — llm:explain_params
			if is_cur and llm.explain_lines then
				local ok, lines = pcall(llm.explain_lines, llm, m)
				if ok and #lines > 0 then
					pv[#pv + 1] = ui.Gap(1)
					pv[#pv + 1] = ui.Line({ ui.Span("parameters"):hl("MareDim") })
					for _, l in ipairs(lines) do
						pv[#pv + 1] = ui.Line({ ui.Span(l):hl("MareMuted") })
					end
				end
			end
			if not can_use(m.provider) then
				local p = llm:provider(m.provider)
				parts = { p and p.login and ("needs login — /login " .. m.provider) or "not configured" }
			end
			items[i] = {
				item_id = m.ref,
				label = (is_cur and "✓ " or "  ") .. m.ref,
				detail = table.concat(parts, " · "),
				preview = ui.Lines(pv),
			}
			if is_cur then
				sel = i
			end
		end
		return list, items, sel, can_use
	end
	local cur = llm:current()
	local models, items, sel, can_use = build_picker_items(cur)
	local list_updated = false
	local function items_differ(a, b)
		if #a ~= #b then
			return true
		end
		for idx = 1, #a do
			if a[idx].item_id ~= b[idx].item_id or a[idx].label ~= b[idx].label or a[idx].detail ~= b[idx].detail then
				return true
			end
		end
		return false
	end
	local unsub
	local _guard <close> = setmetatable({}, {
		__close = function()
			if unsub then
				pcall(unsub)
				unsub = nil
			end
		end,
	})
	local i, _, key, _, chosen_id = ui.select({
		title = "Select model (current: " .. (cur and cur.ref or "none") .. ")",
		items = items,
		selected = sel,
		filter = filter,
		-- Tab: this model's parameters (reasoning effort), then switch to it;
		-- Ctrl+S: switch for this session only (state.json's default stays)
		keys = { { key = "tab", label = "parameters" }, { key = "ctrl+s", label = "this session only" } },
		on_open = function(sel_handle)
			local sync_gen = 0
			local function sync_picker()
				sync_gen = sync_gen + 1
				local gen = sync_gen
				if not sel_handle:is_open() then
					return
				end
				local next_models, next_items, _, next_can_use = build_picker_items(cur)
				if gen ~= sync_gen or not sel_handle:is_open() then
					return
				end
				if items_differ(items, next_items) and sel_handle:update(next_items) then
					models, items, can_use = next_models, next_items, next_can_use
					list_updated = true
				end
			end
			unsub = ctx:on("llm/providers-changed", sync_picker)
			-- Reconcile once right after subscribing so a background refresh that completed
			-- between the initial snapshot and `on_open` is never missed.
			sync_picker()
		end,
	})
	local function resolve_selected()
		if not i then
			return nil
		end
		if type(chosen_id) == "string" and chosen_id ~= "" then
			for _, entry in ipairs(models) do
				if entry.ref == chosen_id then
					return entry
				end
			end
			ui.notify(("Model %s is no longer available"):format(chosen_id), "warning")
			return nil
		end
		if list_updated then
			ui.notify("Model list updated while selecting — please choose again", "warning")
			return nil
		end
		return models[i]
	end
	local E = require("commands.effort")
	if i and key == "tab" then
		local m = resolve_selected()
		if not m then
			return
		end
		local levels = E.levels(m, ctx:get("llm"))
		if not levels then
			ui.notify(m.ref .. " has no parameters to adjust (no reasoning effort)", "info")
			return
		end
		-- the level this model would run at: the one remembered for it, else the current one
		local now = ctx:get("llm"):clamp_effort(m)
		local opts, at = {}, 1
		for k, e in ipairs(levels) do
			opts[k] = { label = e, detail = e == now and "current" or nil }
			if e == now then
				at = k
			end
		end
		local j, _, k2 = ui.select({
			title = "Reasoning effort · " .. m.ref,
			items = opts,
			selected = at,
			keys = { { key = "ctrl+s", label = "this session only" } },
		})
		if not j then
			return
		end
		local session = k2 == "ctrl+s"
		if not (m.ref == (cur and cur.ref)) then
			ctx:get("llm"):set_model(m.ref, { save = not session })
		end
		E.set(ctx, levels[j], { session = session, quiet = true })
		ui.notify(("Model: %s · effort %s%s"):format(m.ref, levels[j], session and " (this session)" or ""))
		return
	end
	if i then
		local m = resolve_selected()
		if not m then
			return
		end
		local mp = ctx:get("llm"):provider(m.provider)
		if not can_use(m.provider) and mp and mp.login then
			-- picking a model of a provider without credentials logs in first
			local ok, err = pcall(ctx:get("llm").login, ctx:get("llm"), m.provider)
			if not ok then
				ui.notify(("%s needs login: %s"):format(m.provider, tostring(err)), "error")
				return
			end
		end
		local session = key == "ctrl+s"
		ctx:get("llm"):set_model(m.ref, { save = not session })
		if session then
			ui.notify(("Model: %s (this session only)"):format(m.ref))
		end
	end
end
