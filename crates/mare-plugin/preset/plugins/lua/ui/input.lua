-- ui.input: input routing (queue, submit, steer, follow-up, Esc) and the welcome line;
-- part of the `ui` plugin (plugins/ui.lua), loaded at the start of its apply.
return function(D)
	local ctx, cfg, U = D.ctx, D.cfg, D.U
	local service, redraw_status = D.service.service, D.service.redraw_status
	ctx:inject({ "commands" }, function(sub)
		sub.commands:register("notifications", {
			description = "Show recent notifications",
			auto_execute = true,
			run = function()
				local rows = {}
				local notifications = U.notification_history()
				for i = #notifications, 1, -1 do
					local notice = notifications[i]
					rows[#rows + 1] = U.Line({ U.Span(notice.level):hl("MareMuted") })
					rows[#rows + 1] = U.Paragraph(notice.text)
					rows[#rows + 1] = U.Gap()
				end
				if #rows == 0 then
					rows[1] = U.Line("No notifications yet")
				end
				U.overlay("Notifications", U.Lines(rows))
			end,
		})
	end)

	---------------------------------------------------------------------------
	-- input routing: the fallback after the ui:handler chains (ordered chains, run by ui.service on
	-- every frontend); a handled event is left alone
	---------------------------------------------------------------------------
	-- Queued messages: Enter while the agent works queues the message; it is shown above the input
	-- until the agent takes it, then appears in the transcript as a normal user message. Ctrl+S
	-- (input/steer) injects it into the running turn instead.
	-- (plugins/lua/ui/queue_edit.lua: the list, ↑ to select one, Enter to edit it in place)
	-- Loaded lazily on first queue activity so idle startups skip `ui.queue_edit`.
	local queue_edit
	local function ensure_queue_edit()
		if not queue_edit then
			queue_edit = require("ui.queue_edit")({ ctx = ctx, U = U, service = service })
		end
		return queue_edit
	end
	local function refresh_queue()
		if queue_edit then
			queue_edit.refresh()
		end
	end
	ctx:on("input/queue-select", function()
		ensure_queue_edit().select()
	end)
	ctx:on("input/queue-move", function(ev)
		ensure_queue_edit().move(ev.delta or 0)
	end)
	ctx:on("input/queue-edit", function()
		ensure_queue_edit().edit()
	end)
	ctx:on("input/queue-cancel", function(ev)
		if queue_edit then
			queue_edit.cancel(ev)
		end
	end)
	ctx:on("input/queue-remove", function()
		ensure_queue_edit().remove()
	end)

	-- Enter on an empty input while messages wait: send now / steer / interrupt and send
	ctx:on("input/queue-menu", function()
		ctx:spawn(function()
			local i = U.select({
				title = "Queued messages",
				items = {
					{ label = "Steer now", detail = "inject them into the running turn" },
					{ label = "Interrupt and send", detail = "stop the current turn, then send them" },
					{ label = "Keep queued", detail = "send when the turn finishes" },
				},
			})
			if not i or i == 3 or not ctx:get("agent") then
				return
			end
			local ok, err = pcall(function()
				local agent_service = ctx:get("agent")
				local lane = i == 1 and "steer" or "follow_up"
				for _, item in ipairs(agent_service:list_queue().data or {}) do
					agent_service:update_queued(item.input_id, { lane = lane })
				end
				if i ~= 1 then
					-- Keep every stable identity durably queued while the current turn stops.
					agent_service:cancel({ clear_queue = false })
					while agent_service:get().busy do
						mare.task.sleep(20)
					end
					agent_service:continue()
				end
			end)
			if not ok then
				U.notify("Queued input was not changed: " .. tostring(err), "error")
			end
			refresh_queue()
		end)
	end)
	ctx:on("queue/changed", function(ev)
		if queue_edit or (type(ev) == "table" and type(ev.items) == "table" and #ev.items > 0) then
			ensure_queue_edit().refresh()
		end
	end)
	ctx:on("item/completed", function(ev)
		if ev and ev.item and ev.item.message then
			refresh_queue()
		end
	end)
	ctx:on("turn/completed", refresh_queue)
	ctx:on("turn/started", refresh_queue)

	-- Submission transfers editor ownership only after durable acceptance. Merge a rejected
	-- attempt ahead of any text/images typed while persistence was waiting.
	local function submit_prompt(text, ev)
		ctx:spawn(function()
			local ok, res = pcall(function()
				return ctx:get("agent"):prompt(text, { images = ev.images, image_markers = ev.image_markers })
			end)
			if not ok then
				local msg = type(res) == "table" and res.message or tostring(res)
				U.notify(msg, "warning")
				if mare.ui.connected() and type(text) == "string" and text ~= "" then
					mare.ui.set_editor(text, { prepend = true, images = ev.images, image_markers = ev.image_markers })
				end
			elseif type(res) == "table" and res.error then
				U.notify(res.error, "warning")
				if mare.ui.connected() and type(text) == "string" and text ~= "" then
					mare.ui.set_editor(text, { prepend = true, images = ev.images, image_markers = ev.image_markers })
				end
			end
		end)
	end

	local function queue_input(method, ev)
		local agent_service = ctx:get("agent")
		local ok, res = pcall(function()
			if method == "steer" then
				return agent_service:steer(ev.text, { images = ev.images, image_markers = ev.image_markers })
			end
			return agent_service:prompt(
				ev.text,
				{ when_busy = "queue", images = ev.images, image_markers = ev.image_markers }
			)
		end)
		local accepted = ok
			and type(res) == "table"
			and (res.status == "queued" or res.status == "started" or res.input_id ~= nil)
		if not accepted then
			local err = ok
					and (type(res) == "table" and (res.error or res.status) or "input acceptance was not acknowledged")
				or (type(res) == "table" and res.message or tostring(res))
			U.set_editor(ev.text, { prepend = true, images = ev.images, image_markers = ev.image_markers })
			U.notify("Input was not queued: " .. tostring(err), "error")
		end
		refresh_queue()
		return accepted
	end

	ctx:on("input/submit", function(ev)
		if ev.handled then
			if queue_edit then
				queue_edit.cancel({ keep_editor = true })
			end
			return
		end
		if not ctx:get("agent") then
			return
		end
		local text = ev.text
		if queue_edit and queue_edit.submit(ev) then
			return
		end
		if string.match(text, "^%s*$") and not (ev.images and #ev.images > 0) then
			return
		end
		if ctx:get("agent"):get().busy then
			queue_input("follow_up", ev)
			return
		end
		submit_prompt(text, ev)
	end)

	-- steer: inject into the running turn (delivered after the current tool calls)
	ctx:on("input/steer", function(ev)
		if queue_edit then
			queue_edit.cancel({ keep_editor = true })
		end
		if not ctx:get("agent") or not ev.text or string.match(ev.text, "^%s*$") then
			return
		end
		if ctx:get("agent"):get().busy then
			queue_input("steer", ev)
		else
			submit_prompt(ev.text, ev)
		end
	end)

	-- kept for compatibility (Alt+Enter): same as Enter while busy — queue
	ctx:on("input/follow-up", function(ev)
		if queue_edit then
			queue_edit.cancel({ keep_editor = true })
		end
		if not ctx:get("agent") then
			return
		end
		if ctx:get("agent"):get().busy then
			queue_input("follow_up", ev)
		else
			submit_prompt(ev.text, ev)
		end
	end)

	-- Esc: interrupt while working; pressed twice quickly while idle, open the /undo picker
	-- .
	-- Runs after the plugins that stop their own work (a running `!cmd`, a compaction): they
	-- set `ev.handled`, and that press is neither an interrupt of the agent nor half a double-Esc.
	local last_esc, undo_hint
	local hint_generation = 0
	local release_undo_hint = U.status("undo-hint", {
		align = "left",
		order = 100,
		keep = true,
		render = function()
			return undo_hint and U.Span(undo_hint):hl("MareDim") or nil
		end,
	})
	ctx:effect(function()
		return release_undo_hint
	end, "undo hint")
	local function clear_undo_hint()
		hint_generation = hint_generation + 1
		last_esc, undo_hint = nil, nil
		U.status_redraw("undo-hint")
	end
	ctx:on("key/interrupt", function(ev)
		if type(ev) == "table" and ev.handled then
			clear_undo_hint()
			return
		end
		local agent_service = ctx:get("agent")
		if agent_service and agent_service:get().busy then
			agent_service:cancel()
			clear_undo_hint()
			return
		end
		-- only with an empty input: Esc on a
		-- draft never rewinds, and /undo would put the old prompt over it
		if type(ev) == "table" and ev.editor_empty == false then
			clear_undo_hint()
			return
		end
		local now = mare.task.clock()
		if last_esc and now - last_esc < cfg.double_esc_ms then
			clear_undo_hint()
			local cmds = ctx:get("commands")
			if cmds then
				-- `/undo` can raise `commands.command_unavailable` (e.g. under `--no-session`);
				-- never let an error escape `key/interrupt` into plugin `problems`.
				local ok, err = pcall(cmds.run, cmds, "/undo")
				if
					not ok
					and err ~= nil
					and not (type(err) == "table" and err.code == "commands.command_unavailable")
				then
					ui.notify(tostring(err), "warning")
				end
			end
		else
			last_esc = now
			local key = (mare.keymap.hint("interrupt", "editor") or {})[1]
			if key then
				local snapshot = service.get_keymap(nil, nil, "editor")
				for _, binding in ipairs(snapshot.bindings or {}) do
					if binding.key == key then
						key = binding.key_label or key
						break
					end
				end
				undo_hint = key .. " again to undo"
				U.status_redraw("undo-hint")
			end
			hint_generation = hint_generation + 1
			local generation = hint_generation
			ctx:spawn(function()
				mare.task.sleep(cfg.double_esc_ms)
				if generation == hint_generation then
					clear_undo_hint()
				end
			end)
		end
	end)

	-- welcome line: local data only, so it goes into the very first frame when the UI is
	-- already attached. It is
	-- the transcript's first entry once per process and stays a live block until
	-- startup is ready (`app/ready`): the provider plugins register after this one, so the
	-- model fills in as they do, and a reload before then (trusting the folder reloads `ui`)
	-- replaces it instead of committing a second copy.
	local WELCOMED = "ui.welcomed" -- mare.g: committed already, a later reload shows none
	local welcome_id
	local welcome_drawn = false
	local welcome_model = nil
	local function current_welcome_model()
		local llm_service = ctx:get("llm")
		local m = llm_service and llm_service:current()
		return m and m.ref or false
	end
	local function welcome_node(model_ref)
		-- `~` and middle elision (`…/uxlab/repo`): the rule shared with the status bar and the
		-- trust question; what's left of the line after `mare <version>  ` and the model
		local cwd = U.display_path(mare.cwd, math.max(16, math.min(48, U.width() - 40)))
		local info = { version = mare.version, cwd = cwd, model = model_ref or nil }
		-- the pixel logo only opens a new, empty session (Neovim intro); resumed
		-- sessions, narrow / 16-color terminals, ascii icons and logo = "compact" get one line
		local o = mare.o or mare.config or {}
		local args = mare.args or {}
		local logo = o.logo
		if logo == nil then
			logo = true
		end
		local resumed = args.continue or args.resume ~= nil
		if logo == false then
			return nil
		end
		local full = logo == true
			and not resumed
			and U.width() >= (ui.Logo.min_width or 52)
			and ui.color_depth() ~= "16"
			and ui.color_depth() ~= "none"
			and (U.icon_set and U.icon_set() or "unicode") ~= "ascii"
		return ui.Lines({ full and ui.Logo:render(info) or ui.Logo:compact(info), ui.Gap() })
	end
	local function welcome(skip_status)
		if mare.g[WELCOMED] then
			return
		end
		local model_ref = current_welcome_model()
		if welcome_drawn and model_ref == welcome_model then
			return
		end
		welcome_drawn = true
		welcome_model = model_ref
		local node = welcome_node(model_ref)
		if welcome_id then
			U.update(welcome_id, { node = node or ui.Lines({}) })
		elseif node then
			welcome_id = U.push("info", { node = node })
		end
		if not skip_status then
			redraw_status()
		end
	end
	if U.connected() then
		welcome()
	end
	ctx:on("llm/providers-changed", function()
		welcome(true)
	end)
	ctx:on("llm/model-changed", function()
		welcome(true)
	end)
	ctx:on("app/ready", function()
		welcome(true)
		redraw_status()
		if welcome_id then
			U.finish(welcome_id)
		end
		mare.g[WELCOMED] = true
		welcome_id = nil
	end, { prepend = true })
	ctx:effect(function()
		return function()
			if welcome_id then
				U.remove(welcome_id)
			end
		end
	end, "ui welcome")
end
