-- Built-in `conversations` service: ephemeral, parent-isolated side conversations branched
-- from committed SES-FORK history (`session:snapshot_context`), plus `/side` & `/btw` routing.
return {
	name = "conversations",
	tags = { "minimal" },
	inject = { "agent", "llm", "tools", "bus", "policy", "task" },
	provide = { "conversations" },
	apply = function(ctx)
		local lib = require("mare_lib")
		local follow_session = lib.follow(ctx, "session")
		local follow_context = lib.follow(ctx, "context")
		local loop = require("conversations.side_loop")
		local side_parent_mod = require("conversations.side_parent")

		local SAFE_SIDE_COMMANDS = {
			btw = true,
			help = true,
			session = true,
			side = true,
			status = true,
		}

		local current
		local live = true
		local side_calls = {}
		local bypass_get_route = 0
		local widget = "conversations:side"
		local side_turns = {}
		local side_turns_live = {}
		local side_turn_order = {}

		local function project_value(v)
			if mare.projection and type(mare.projection.project) == "function" then
				return mare.projection.project(v, { preserve_invalid = true })
			end
			return v
		end

		local function session_id()
			local sess = follow_session()
			local info = sess and sess:info()
			return info and info.session_id
		end

		local function ui_connected()
			local ok, connected = pcall(mare.ui.connected)
			return ok and connected == true
		end

		local function hide()
			if mare.ui and lib.callable(mare.ui.set_widget) then
				pcall(mare.ui.set_widget, widget, "above-editor", nil)
			end
		end

		local function notify_ui(msg, level)
			if mare.ui and lib.callable(mare.ui.notify) then
				pcall(mare.ui.notify, msg, level or "info")
			end
		end

		local function is_parent_busy()
			local ag = ctx:get("agent")
			if not ag then
				return false
			end
			bypass_get_route = bypass_get_route + 1
			local _bypass_guard <close> = lib.guard(function()
				bypass_get_route = bypass_get_route - 1
			end)
			local ok, st = pcall(ag.get, ag)
			return ok and type(st) == "table" and st.busy == true or false
		end

		local function is_side_call(id)
			if type(id) ~= "string" then
				return false
			end
			if side_calls[id] == true then
				return true
			end
			local root = id:match("^(.-)/nested/")
			return root ~= nil and side_calls[root] == true
		end

		local parent
		parent = side_parent_mod(ctx, function(id)
			return (id and is_side_call(id)) or (id == nil and next(side_calls) ~= nil)
		end, function()
			if current then
				ctx.bus:emit("conversation/changed", {
					id = current.id,
					session_id = current.owner,
					selected = current.selected,
					parent_status = parent.get(),
				})
			end
		end, is_parent_busy)

		local function render(side)
			if current ~= side or not side.selected or not ui_connected() then
				return
			end
			if not (mare.ui and lib.callable(mare.ui.set_widget) and lib.callable(mare.ui.Lines)) then
				return
			end
			local visible = loop.visible(side)
			local text = visible ~= "" and visible or "Ready for a side question"
			local p = parent.get()
			local rows = {
				mare.ui.Line("SIDE · Esc / /side back to return and discard · /side close to discard"),
				mare.ui.Line("Parent: " .. p.state .. (p.detail and (" — " .. p.detail) or "")),
				mare.ui.Clip(mare.ui.Markdown(text), 12),
			}
			if side.tool then
				rows[#rows + 1] = mare.ui.Line("Running: " .. tostring(side.tool.name or "tool"))
			end
			if side.tool_output and side.tool_output ~= "" then
				rows[#rows + 1] = mare.ui.Clip(mare.ui.Markdown(side.tool_output), 3)
			end
			pcall(mare.ui.set_widget, widget, "above-editor", mare.ui.Lines(rows), 100)
		end

		local function settle_side_turn(side, turn_id, res, started_at_ms)
			local live_turn = side_turns_live[turn_id]
			side_turns_live[turn_id] = nil
			local err_msg = res and res.error or nil
			local status = err_msg == "cancelled" and "cancelled" or (err_msg ~= nil and "failed" or "completed")
			local termination_reason = err_msg == "cancelled" and "aborted"
				or (err_msg ~= nil and "error" or "completed")
			local msgs = {}
			if side and type(side.messages) == "table" then
				for _, m in ipairs(side.messages) do
					msgs[#msgs + 1] = project_value(lib.copy_deep(m))
				end
			end
			local err_code = err_msg == "cancelled" and "cancelled" or "error"
			local err_kind = err_msg == "cancelled" and "aborted" or (err_msg ~= nil and "error" or nil)
			local snap = {
				turn_id = turn_id,
				status = status,
				termination_reason = termination_reason,
				error = lib.structured_error(err_msg, err_code),
				error_kind = err_kind,
				usage = res and res.usage and project_value(res.usage) or nil,
				text = project_value((res and res.text) or ""),
				messages = mare.json.array(msgs),
				started_at_ms = (live_turn and live_turn.started_at_ms) or started_at_ms or lib.to_epoch_ms(
					mare.task.now()
				),
				completed_at_ms = lib.to_epoch_ms(mare.task.now()),
			}
			side_turns[turn_id] = snap
			while #side_turn_order > 128 do
				local oldest = table.remove(side_turn_order, 1)
				if side_turns_live[oldest] == nil then
					side_turns[oldest] = nil
				end
			end
			if live_turn then
				for _, waiter in ipairs(live_turn.waiters) do
					waiter:send(snap)
				end
			end
			return snap
		end

		local function queue_item_view(item)
			return {
				input_id = item.input_id or item.turn_id,
				lane = "follow_up",
				text = type(item.text) == "string" and item.text or "",
				images = mare.json.array({}),
			}
		end

		local function drain_pending_queue(side, reason)
			local removed = {}
			while #side.queue > 0 do
				local item = table.remove(side.queue, 1)
				removed[#removed + 1] = queue_item_view(item)
				ctx.bus:emit("conversation/settled", {
					id = side.id,
					session_id = side.owner,
					turn_id = item.turn_id,
					prompt = project_value(item.text),
					text = "",
					error = lib.structured_error(reason, reason == "cancelled" and "cancelled" or "error"),
				})
				settle_side_turn(side, item.turn_id, { text = "", error = reason }, item.started_at_ms)
			end
			return removed
		end

		local function clear_settled_side_turns()
			for tid in pairs(side_turns) do
				if side_turns_live[tid] == nil then
					side_turns[tid] = nil
				end
			end
			local kept = {}
			for _, tid in ipairs(side_turn_order) do
				if side_turns_live[tid] ~= nil then
					kept[#kept + 1] = tid
				end
			end
			side_turn_order = kept
		end

		local function report_side_turn_result(r)
			if
				type(r) == "table"
				and r.error
				and r.status ~= "cancelled"
				and not (type(r.error) == "table" and r.error.code == "cancelled")
			then
				notify_ui("Side conversation: " .. (lib.error_message(r.error) or tostring(r.error)), "warning")
			end
		end

		local function close()
			local side = current
			current = nil
			local removed = {}
			local active_turn_id = side and side.turn_id or nil
			if side then
				side.cancel:cancel()
				if side.turn_cancel then
					side.turn_cancel:cancel()
				end
				removed = drain_pending_queue(side, "cancelled")
				if active_turn_id and side_turns_live[active_turn_id] then
					settle_side_turn(side, active_turn_id, { text = side.text or "", error = "cancelled" })
				end
				ctx.bus:emit("conversation/closed", { id = side.id, session_id = side.owner })
			end
			clear_settled_side_turns()
			hide()
			return active_turn_id, removed
		end

		ctx:effect(function()
			return function()
				live = false
				close()
			end
		end, "conversations lifecycle")

		ctx:on("session/changed", close)
		ctx:on("session/loaded", close)

		local function active()
			return current ~= nil and current.selected and current.owner == session_id()
		end

		local service = {}

		function service.info(_, _)
			local side = current
			if not side then
				return nil
			end
			return {
				conversation_id = side.id,
				session_id = side.owner,
				selected = side.selected,
				busy = side.busy,
				model = side.model and side.model.ref or nil,
				params = side.params and lib.copy_deep(side.params) or {},
				message_count = #side.messages,
				parent_status = parent.get(),
				turn_id = side.busy and side.turn_id or nil,
			}
		end

		function service.close(_, _)
			close()
			return true
		end

		function service.cancel(_, _)
			if current and current.turn_cancel then
				current.turn_cancel:cancel()
				return true
			end
			return false
		end

		function service.toggle(_, _)
			if not current then
				return nil, "No side conversation is open"
			end
			current.selected = not current.selected
			ctx.bus:emit("conversation/changed", {
				id = current.id,
				session_id = current.owner,
				selected = current.selected,
				parent_status = parent.get(),
			})
			if current.selected then
				render(current)
			else
				hide()
			end
			return service.info(service, ctx)
		end

		local function build_side(opts)
			if not live then
				return nil, "Conversation owner was disposed"
			end
			if current then
				return nil, "A side conversation is already open; close it or switch back"
			end
			local sess = follow_session()
			if not sess then
				return nil, "Session service is unavailable"
			end
			if sess:switching() then
				return nil, "Session switch in progress; retry /side afterwards"
			end
			local owner_id = session_id()
			local agent_spi = ctx.agent
			local agent_msgs = agent_spi:messages() or {}
			local snap_ctx = sess:snapshot_context({
				session_id = owner_id,
				before_item_id = opts and opts.before_item_id or nil,
				model = opts and opts.model or nil,
				effort = opts and opts.effort or nil,
				allow_memory = true,
			})
			if not snap_ctx or #(snap_ctx.messages or {}) == 0 and #agent_msgs == 0 then
				return nil, "Send a main conversation message before starting /side"
			end
			local cur_model = ctx.llm:current()
			local model_ref = snap_ctx and snap_ctx.model or (cur_model and cur_model.ref)
			local resolved_model = model_ref and ctx.llm:resolve(model_ref) or cur_model
			if not resolved_model then
				return nil, "No model available"
			end
			resolved_model = lib.copy_deep(resolved_model)
			local effort = (opts and opts.effort) or (snap_ctx and snap_ctx.effort) or nil
			local resolved_params = ctx.llm:resolve_params(resolved_model, effort and { effort = effort } or nil).params
			local act = ctx.agent.activity and ctx.agent:activity() or {}
			local request = type(act) == "table" and act.request or nil
			local context_svc = follow_context()
			local system = (request and type(request.system) == "string" and request.system)
				or (context_svc and context_svc:system_prompt(resolved_model))
				or ""
			-- Keep tool_names nil so newly registered tools are bound cleanly;
			-- side_calls + tool/before enforces confined read-only execution.
			local tool_names = nil
			local messages = loop.seed(snap_ctx, act)
			local side = {
				id = mare.crypto.random_hex(8),
				owner = owner_id,
				selected = true,
				busy = false,
				model = resolved_model,
				params = resolved_params or {},
				effort = effort,
				system = system,
				tool_names = tool_names,
				messages = messages,
				calls = side_calls,
				queue = {},
				worker_running = false,
				cancel = mare.task.token(),
				turn_cancel = nil,
				text = "",
				partial = "",
			}
			side.boundary = #side.messages
			return side
		end

		function service.start(_, _, opts)
			local ok, side_or_err, err_msg = pcall(build_side, opts)
			if not ok then
				local msg = type(side_or_err) == "table" and side_or_err.message or tostring(side_or_err)
				return nil, msg
			end
			if not side_or_err then
				return nil, err_msg
			end
			current = side_or_err
			ctx.bus:emit("conversation/opened", {
				id = current.id,
				session_id = current.owner,
				model = current.model and current.model.ref or nil,
			})
			render(current)
			return service.info(service, ctx)
		end

		local function run_queue(side)
			if side.worker_running then
				return
			end
			local _worker_guard <close> = lib.guard(function()
				side.busy = false
				side.worker_running = false
			end)
			side.worker_running = true
			while current == side and not side.cancel:is_cancelled() and #side.queue > 0 do
				local item = table.remove(side.queue, 1)
				side.busy = true
				side.turn_id = item.turn_id
				local turn_cancel = side.cancel:child()
				side.turn_cancel = turn_cancel
				side.text = ""
				side.partial = ""
				local last_draw = 0
				local function draw(ev)
					ctx.bus:emit("conversation/stream", {
						id = side.id,
						session_id = side.owner,
						turn_id = item.turn_id,
						event = project_value(ev),
					})
					if type(ev) == "table" then
						if ev.type == "text_delta" then
							local d = project_value(tostring(ev.delta or ""))
							side.text = side.text .. d
							side.partial = side.partial .. d
						elseif ev.type == "tool_start" then
							side.tool = ev.call
							side.tool_output = nil
						elseif ev.type == "tool_output" then
							local out = ev.partial and (ev.partial.output or ev.partial.content)
							if type(out) == "string" then
								side.tool_output =
									mare.text.cut(project_value(out), 8192, { keep = "tail", unit = "bytes" })
							end
						elseif ev.type == "tool_result" then
							side.tool = nil
							side.tool_output = nil
						end
					end
					local now = mare.task.clock()
					if now - last_draw >= 50 then
						last_draw = now
						render(side)
					end
				end

				local ok, result = pcall(loop.run, ctx, side, item.text, item.opts.images, draw, turn_cancel)
				local res = ok and result or { error = project_value(tostring(result)), text = side.text }
				side.turn_cancel = nil
				side.turn_id = nil
				if #side.queue == 0 or side.cancel:is_cancelled() or res.error ~= nil then
					side.busy = false
				end
				ctx.bus:emit("conversation/settled", {
					id = side.id,
					session_id = side.owner,
					turn_id = item.turn_id,
					prompt = project_value(item.text),
					text = project_value(res.text or ""),
					message = res.final_message and project_value(res.final_message) or nil,
					error = lib.structured_error(res.error, res.error == "cancelled" and "cancelled" or "error"),
				})
				settle_side_turn(side, item.turn_id, res, item.started_at_ms)
				render(side)
				if res.error ~= nil or side.cancel:is_cancelled() then
					local reason = (res.error == "cancelled" or side.cancel:is_cancelled()) and "cancelled"
						or ("previous side turn failed: " .. tostring(res.error))
					drain_pending_queue(side, reason)
					break
				end
			end
			side.busy = false
			side.worker_running = false
		end

		local function enqueue_side_turn(text, opts)
			opts = type(opts) == "table" and opts or {}
			local side = current
			if not side or side.owner ~= session_id() or side.cancel:is_cancelled() then
				lib.fail("agent.rejected", "no side conversation is open")
			end
			if side.busy and opts.when_busy == "reject" then
				lib.fail("agent.rejected", "side conversation is busy")
			end
			local str_text = type(text) == "string" and text or ""
			if
				str_text:match("^%s*$")
				and not (opts.images and #opts.images > 0)
				and not (opts.content and #opts.content > 0)
			then
				lib.fail("invalid_arguments", "prompt input must not be empty")
			end
			local turn_id = (type(opts.turn_id) == "string" and opts.turn_id ~= "") and opts.turn_id
				or mare.crypto.random_hex(8)
			local input_id = mare.crypto.random_hex(8)
			local was_busy = side.busy or side.worker_running
			local now = lib.to_epoch_ms(mare.task.now())
			side_turns_live[turn_id] = {
				turn_id = turn_id,
				started_at_ms = now,
				waiters = {},
			}
			side_turn_order[#side_turn_order + 1] = turn_id
			side.queue[#side.queue + 1] = {
				turn_id = turn_id,
				input_id = input_id,
				started_at_ms = now,
				text = str_text,
				opts = lib.copy_deep(opts),
			}
			if not side.worker_running then
				side.busy = true
				side.turn_id = turn_id
				ctx:spawn(function()
					run_queue(side)
				end)
			end
			local out = {
				status = was_busy and "queued" or "started",
				turn_id = turn_id,
			}
			if was_busy then
				out.input_id = input_id
			end
			return out
		end

		local function wait_side_turn(turn_id, opts)
			opts = type(opts) == "table" and opts or {}
			local target_id = (type(turn_id) == "string" and turn_id ~= "") and turn_id
				or (current and (current.turn_id or side_turn_order[#side_turn_order]))
			if not target_id or target_id == "" then
				lib.fail("invalid_arguments", "no active or completed side turn to wait on")
			end
			local settled = side_turns[target_id]
			if settled then
				return settled
			end
			local live_turn = side_turns_live[target_id]
			if not live_turn then
				lib.fail("invalid_arguments", "unknown turn_id: " .. tostring(target_id))
			end
			local tx, rx = mare.task.channel()
			live_turn.waiters[#live_turn.waiters + 1] = tx
			if side_turns[target_id] then
				return side_turns[target_id]
			end
			if type(opts.timeout_ms) == "number" then
				local ok_wait, rec = mare.task.timeout(math.floor(opts.timeout_ms), function()
					return rx:recv()
				end)
				if not ok_wait then
					return {
						turn_id = target_id,
						status = "running",
						timed_out = true,
						started_at_ms = live_turn.started_at_ms,
					}
				end
				return rec
			end
			return rx:recv()
		end

		-- Side-conversation tool policy: confined read-only tools only (including nested `call_tool` calls).
		ctx.policy:voter("tool/before", function(call, info)
			if call and is_side_call(call.id) then
				if (info and info.side_effect or "external") ~= "read" or (info and info.unconfined) then
					return { decision = "deny", reason = "Side conversations require confined read-only exploration" }
				end
				return {
					decision = "allow",
					sandbox = { level = "read-only", network = false },
				}
			end
		end, { prepend = true })

		-- Frontend routing interception (`prepend = true`): only claims when side is focused
		-- (or when waiting on a known side turn_id).
		ctx:on("agent/frontend-route", function(req)
			if type(req) ~= "table" then
				return
			end
			local op = req.op
			if op == "wait" then
				local opts = type(req.opts) == "table" and req.opts or {}
				local tid = type(opts.turn_id) == "string" and opts.turn_id or nil
				if
					(tid ~= nil and (side_turns[tid] ~= nil or side_turns_live[tid] ~= nil))
					or (tid == nil and active())
				then
					local ok, res = pcall(wait_side_turn, tid, opts)
					if not ok then
						return { handled = true, error = res }
					end
					return { handled = true, result = res }
				end
				return
			end
			if op == "cancel" then
				local opts = type(req.opts) == "table" and req.opts or {}
				local tid = type(opts.turn_id) == "string" and opts.turn_id ~= "" and opts.turn_id or nil
				if tid ~= nil then
					local side = current
					local matches_live = side ~= nil and side.turn_id == tid
					local queued_idx
					if side then
						for i, item in ipairs(side.queue) do
							if item.turn_id == tid then
								queued_idx = i
								break
							end
						end
					end
					if matches_live or queued_idx ~= nil or side_turns_live[tid] ~= nil then
						local removed = {}
						if queued_idx ~= nil then
							local item = table.remove(side.queue, queued_idx)
							removed[#removed + 1] = queue_item_view(item)
							ctx.bus:emit("conversation/settled", {
								id = side.id,
								session_id = side.owner,
								turn_id = item.turn_id,
								prompt = project_value(item.text),
								text = "",
								error = lib.structured_error("cancelled", "cancelled"),
							})
							settle_side_turn(side, item.turn_id, { text = "", error = "cancelled" }, item.started_at_ms)
						elseif matches_live then
							if side.turn_cancel then
								side.turn_cancel:cancel()
							end
							if opts.clear_queue ~= false then
								removed = drain_pending_queue(side, "cancelled")
							end
						elseif side_turns_live[tid] ~= nil then
							settle_side_turn(side, tid, { text = "", error = "cancelled" })
						end
						return {
							handled = true,
							result = {
								turn_id = tid,
								removed = mare.json.array(removed),
							},
						}
					end
					return
				end
			end
			if not active() then
				return
			end
			if op == "prompt" then
				local ok, res = pcall(enqueue_side_turn, req.text, req.opts)
				if not ok then
					return { handled = true, error = res }
				end
				return { handled = true, result = res }
			elseif op == "steer" or op == "follow_up" then
				local ok, enq = pcall(enqueue_side_turn, req.text, req.opts)
				if not ok then
					return { handled = true, error = enq }
				end
				return { handled = true, result = { input_id = enq.input_id or enq.turn_id } }
			elseif op == "busy" then
				return { handled = true, result = current.busy }
			elseif op == "state" or op == "get" then
				if bypass_get_route > 0 then
					return
				end
				return {
					handled = true,
					result = {
						busy = current.busy,
						turn_id = current.busy and current.turn_id or nil,
						session_id = current.owner,
						queued = #current.queue,
						message_count = #current.messages,
						model = current.model and current.model.ref or nil,
						effort = current.effort,
						turn_cancelled = current.turn_cancel ~= nil and current.turn_cancel:is_cancelled() or false,
						interrupted = false,
					},
				}
			elseif op == "continue" then
				return {
					handled = true,
					error = {
						code = "agent.rejected",
						message = "Send a side question, or /side back to leave the side conversation",
					},
				}
			elseif op == "cancel" then
				local cancelled_turn, removed = close()
				local out = { removed = mare.json.array(removed or {}) }
				if cancelled_turn ~= nil then
					out.turn_id = cancelled_turn
				end
				return {
					handled = true,
					result = out,
				}
			end
		end, true)

		for _, event in ipairs({ "input/steer", "input/follow-up" }) do
			ctx:on(event, function(ev)
				if type(ev) == "table" and not ev.handled and active() then
					ev.handled = true
					pcall(enqueue_side_turn, ev.text or "", { images = ev.images })
				end
			end, true)
		end

		-- Ordered submit & interrupt isolation when optional `ui` is loaded.
		ctx:inject({ "ui" }, function(sub)
			sub.ui:handler("submit", "conversations", { order = 15 }, function(ev)
				if type(ev) ~= "table" or type(ev.text) ~= "string" or not active() then
					return false
				end
				local trimmed = ev.text:match("^%s*(.-)%s*$")
				if trimmed == "" and not (ev.images and #ev.images > 0) then
					return true
				end
				local cmd = trimmed:match("^/([%w%-_:]+)")
				if cmd then
					if SAFE_SIDE_COMMANDS[cmd] then
						return false
					end
					local reason = ("/%s is not available in a side conversation"):format(cmd)
					ev.command_result = { ok = false, status = "rejected", reason = reason, error = reason }
					notify_ui(reason, "warning")
					if mare.ui and lib.callable(mare.ui.set_editor) and ui_connected() then
						pcall(mare.ui.set_editor, trimmed, { keep_draft = true })
					end
					return true
				end
				if trimmed:sub(1, 1) == "!" then
					local reason = "Shell escapes (!) are not available in a side conversation"
					ev.command_result = { ok = false, status = "rejected", reason = reason, error = reason }
					notify_ui(reason, "warning")
					if mare.ui and lib.callable(mare.ui.set_editor) and ui_connected() then
						pcall(mare.ui.set_editor, trimmed, { keep_draft = true })
					end
					return true
				end
				ctx:spawn(function()
					local ok, started_or_err = pcall(enqueue_side_turn, trimmed, { images = ev.images })
					if not ok then
						notify_ui(
							"Side conversation: " .. (lib.error_message(started_or_err) or tostring(started_or_err)),
							"warning"
						)
						return
					end
					local r = wait_side_turn(started_or_err.turn_id)
					report_side_turn_result(r)
				end)
				return true
			end)

			sub.ui:handler("interrupt", "conversations", { order = 5 }, function(ev)
				if active() and (type(ev) ~= "table" or ev.editor_empty ~= false) then
					close()
					return true
				end
				return false
			end)
		end)

		-- Unified `/side` and `/btw` command entrypoint.
		local function run_side_command(cmd_name, args)
			local trimmed = tostring(args or ""):match("^%s*(.-)%s*$")
			if trimmed == "close" or trimmed == "back" then
				if not current then
					notify_ui("No side conversation is open", "warning")
					return
				end
				close()
				return
			end
			if trimmed == "toggle" then
				local res, err = service.toggle(service, ctx)
				if not res then
					notify_ui(err or "No side conversation is open", "warning")
				end
				return
			end
			if current and current.selected then
				notify_ui(
					"A side conversation is already open; /" .. cmd_name .. " back returns and discards",
					"warning"
				)
				if trimmed ~= "" and mare.ui and lib.callable(mare.ui.set_editor) and ui_connected() then
					pcall(mare.ui.set_editor, trimmed, { keep_draft = true })
				end
				return
			end
			if current then
				close()
			end
			local started, err = service.start(service, ctx)
			if not started then
				notify_ui(err or "Could not start side conversation", "warning")
				if mare.ui and lib.callable(mare.ui.set_editor) and ui_connected() then
					pcall(
						mare.ui.set_editor,
						"/" .. cmd_name .. (trimmed ~= "" and (" " .. trimmed) or ""),
						{ prepend = true }
					)
				end
				return
			end
			if trimmed ~= "" then
				ctx:spawn(function()
					local ok, started_or_err = pcall(enqueue_side_turn, trimmed)
					if not ok then
						notify_ui(
							"Side conversation: " .. (lib.error_message(started_or_err) or tostring(started_or_err)),
							"warning"
						)
						return
					end
					local r = wait_side_turn(started_or_err.turn_id)
					report_side_turn_result(r)
				end)
			end
		end

		ctx:inject({ "commands" }, function(sub)
			for _, cmd_name in ipairs({ "side", "btw" }) do
				sub.commands:register(cmd_name, {
					description = "Open or control an ephemeral side conversation (/"
						.. cmd_name
						.. " [<question> | back | close])",
					run = function(args)
						run_side_command(cmd_name, args)
					end,
				})
			end
		end)

		lib.service(ctx, "conversations", service)
	end,
}
