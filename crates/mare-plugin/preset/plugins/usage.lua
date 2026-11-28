-- usage: token + cost accounting per session; `/usage` shows a breakdown with the subscription
-- quota windows.
--
-- The totals belong to the session: a new session starts at 0; resuming one (in-process
-- /resume, `-c`, `-r`, a fork) recounts them from every assistant message in its file, so
-- nothing carries over from the session before. Requests outside the conversation (compaction
-- summaries, /btw) are added when the llm service reports them with `llm/usage` and are
-- recorded as custom entries in the originating session, so a resume recounts them too.
return {
	name = "usage",
	inject = { "llm", "bus" },
	provide = { "usage" },
	apply = function(ctx)
		-- the optional session service, read on each call
		local session = require("mare_lib").follow(ctx, "session")
		local U = {
			input = 0,
			output = 0,
			cache_read = 0,
			cache_write = 0,
			cache_write_1h = 0,
			cost = 0,
			cost_unknown = false,
			requests = 0,
			-- requests billed to a subscription (ChatGPT plan): no per-token cost
			subscription_requests = 0,
			-- requests outside the conversation (compaction summaries …), included above
			side_requests = 0,
			last_context = 0,
			last_cache_read = 0, -- prompt-cache prefix of the last request (compaction prunes after it)
		}
		local limits = {} -- provider -> latest rate-limit snapshot {primary, secondary, …, at}
		local unassigned = {} -- late usage is kept in memory, never attributed to a different session
		local mare_lib = require("mare_lib")
		local usage_cost = mare_lib.usage_cost

		local function warn_unsaved(detail)
			local text = "Usage was not saved and will be lost" .. (detail and (": " .. tostring(detail)) or "")
			mare.log("warn", text)
			if mare.ui.connected() then
				ui.notify(text, "warning")
			end
		end

		local function zero()
			for k in pairs(U) do
				if k == "cost_unknown" then
					U[k] = false
				elseif k == "reasoning" then
					U[k] = nil
				else
					U[k] = 0
				end
			end
		end

		--- add one request's usage; `ctx_too` = it occupies the conversation's window
		local function add(u, model, subscription, side)
			U.input = U.input + (u.input or 0)
			U.output = U.output + (u.output or 0)
			if type(u.reasoning) == "number" then
				U.reasoning = (U.reasoning or 0) + u.reasoning
			end
			U.cache_read = U.cache_read + (u.cache_read or 0)
			U.cache_write = U.cache_write + (u.cache_write or 0)
			U.cache_write_1h = U.cache_write_1h + (u.cache_write_1h or 0)
			U.requests = U.requests + 1
			if subscription then
				U.subscription_requests = U.subscription_requests + 1
			else
				local req_cost = usage_cost(model, u, false)
				if req_cost ~= nil then
					U.cost = U.cost + req_cost
				else
					U.cost_unknown = true
				end
			end
			if side then
				U.side_requests = U.side_requests + 1
			end
		end

		local function model_of(m)
			return ctx.llm:resolve((m.provider_id or "") .. "/" .. (m.model_id or "")) or ctx.llm:current()
		end

		local function context_of(u)
			return (u.input or 0) + (u.cache_read or 0) + (u.cache_write or 0) + (u.output or 0)
		end

		local function snapshot_limits()
			if next(limits) == nil then
				return nil
			end
			local rate = {}
			for provider, snapshot in pairs(limits) do
				rate[provider] = table.copy(snapshot)
			end
			return rate
		end

		local function emit_changed()
			local payload = table.copy(U)
			local rate = snapshot_limits()
			if rate ~= nil then
				payload.rate_limits = rate
			end
			ctx.bus:emit("usage/changed", payload)
		end

		-- provider-only helpers stay local: the contract is the service's whole surface
		local function reset()
			zero()
			emit_changed()
		end

		local usage = {}
		function usage.totals(_, _)
			return U
		end
		--- tokens occupying the context window after the last request
		function usage.context_tokens(_, _)
			return U.last_context
		end
		function usage.set_context(_, _, n, opts)
			U.last_context = n
			if not (opts and opts.notify == false) then
				emit_changed()
			end
		end
		--- rate_limits() -> { [provider] = {primary?, secondary?, credits?, limit_name?, reached?, received_at_ms} }
		function usage.rate_limits(_, _)
			return limits
		end
		--- get() -> {totals, context_tokens, rate_limits}: one detached snapshot
		function usage.get(_, _)
			return { totals = table.copy(U), context_tokens = U.last_context, rate_limits = snapshot_limits() or {} }
		end
		--- cost_text() -> "$0.1234", "subscription, not billed", or both when mixed
		function usage.cost_text(_, _, fmt)
			fmt = fmt or "$%.4f"
			local billed = U.requests - U.subscription_requests
			if U.subscription_requests > 0 and billed == 0 then
				return "subscription, not billed"
			end
			local s = (U.cost_unknown and U.cost == 0) and "unknown" or fmt:format(U.cost)
			if U.cost_unknown and U.cost > 0 then
				s = s .. " (incomplete)"
			end
			if U.subscription_requests > 0 then
				s = s
					.. (" (+%d subscription request%s, not billed)"):format(
						U.subscription_requests,
						U.subscription_requests == 1 and "" or "s"
					)
			end
			return s
		end
		ctx:on("item/completed", function(ev)
			if ev and ev.status == "failed" and not (ev.item and ev.item.entry_id) then
				return
			end
			local m = ev and ev.item and ev.item.message
			if not m or m.role ~= "assistant" or not m.usage then
				return
			end
			local u = m.usage
			add(u, model_of(m), m.subscription)
			if mare_lib.has_usage_counts(u) then
				U.last_context = context_of(u)
				U.last_cache_read = u.cache_read or 0
			end
			emit_changed()
		end)
		-- requests outside the conversation (llm reports them; compaction summaries, /btw)
		ctx:on("llm/usage", function(ev)
			if type(ev) ~= "table" or type(ev.usage) ~= "table" or ev.purpose == "agent" then
				return
			end
			local info = session() and session().info and session():info() or nil
			if ev.session_id and (not info or ev.session_id ~= info.session_id) then
				unassigned[#unassigned + 1] = table.copy(ev)
				warn_unsaved("the originating session is no longer active")
				return
			end
			local model = ev.model_ref and ctx.llm:resolve(ev.model_ref) or ctx.llm:current()
			add(ev.usage, model, ev.subscription, true)
			if not info or not ev.session_id then
				warn_unsaved("the originating session is unknown")
			elseif not info.ephemeral and not info.readonly then
				local model_id = type(ev.model_ref) == "string" and (ev.model_ref:match("^[^/]+/(.+)$") or ev.model_ref)
					or ""
				local data = {
					id = mare.crypto.uuid(),
					leaf_entry_id = ev.leaf_entry_id or mare.json.null,
					purpose = ev.purpose or "other",
					provider_id = ev.provider_id or "",
					model_id = model_id,
					subscription = ev.subscription == true,
					usage = ev.usage,
				}
				local function write()
					local sess = session()
					if not (sess and sess.add_record) then
						return nil, "session recorder is unavailable"
					end
					local ok, saved = pcall(sess.add_record, sess, "usage/side-v1", data, { persist = true })
					if not ok then
						return nil, saved
					end
					return saved ~= nil, nil
				end
				local saved, err = write()
				if not saved then
					saved, err = write()
				end
				if not saved then
					warn_unsaved(err)
				end
			end
			emit_changed()
		end)
		ctx:on("llm/rate-limits", function(ev)
			if type(ev) == "table" and type(ev.limits) == "table" then
				local lib = require("mare_lib")
				local snap = table.copy(ev.limits)
				snap.received_at_ms = lib.to_epoch_ms(snap.received_at_ms) or lib.to_epoch_ms(mare.task.now())
				limits[ev.provider or "?"] = snap
				emit_changed()
			end
		end)

		--- the session's messages: every assistant message in its file (all branches: tokens
		--- spent on an abandoned branch were still spent), else the restored context
		local function session_messages(ev)
			local ok, entries = false, nil
			if session() and session().entries then
				ok, entries = pcall(session().entries, session())
			end
			if ok and type(entries) == "table" and #entries > 0 then
				local out = {}
				for _, e in ipairs(entries) do
					if e.type == "message" and e.message then
						out[#out + 1] = e.message
					end
				end
				return out
			end
			return ev.messages or {}
		end

		ctx:on("session/loaded", function(ev)
			ev = ev or {}
			-- /tree navigation stays in the same session: the totals stand, the window changes
			if not ev.navigated then
				zero()
				for _, m in ipairs(session_messages(ev)) do
					if m.role == "assistant" and type(m.usage) == "table" then
						add(m.usage, model_of(m), m.subscription)
					end
				end
				if session() and session().list_records then
					for _, side in ipairs(require("mare_lib").all_records(session(), "usage/side-v1") or {}) do
						if type(side) == "table" and type(side.usage) == "table" then
							add(side.usage, model_of(side), side.subscription, true)
						end
					end
				end
			end
			-- the context from the last assistant usage in the restored history; when that usage
			-- predates the latest compaction or projection in the session log, a local estimate of
			-- the restored messages (a context-size owner such as compact refines it afterwards)
			U.last_context, U.last_cache_read = 0, 0
			if mare_lib.session_usage_stale(session(), ev) then
				U.last_context = mare_lib.estimate_messages(ev.messages)
			else
				for i = #(ev.messages or {}), 1, -1 do
					local m = ev.messages[i]
					if m.role == "assistant" and mare_lib.has_usage_counts(m.usage) then
						U.last_context = context_of(m.usage)
						U.last_cache_read = m.usage.cache_read or 0
						break
					end
				end
			end
			emit_changed()
		end)
		ctx:on("session/new", function()
			reset()
		end)

		local function k(n)
			return n >= 1000 and ("%.1fk"):format(n / 1000) or tostring(n)
		end

		--- "5h" / "weekly" / "2d" for a window length in minutes
		local function window_name(mins)
			if not mins then
				return "window"
			end
			if mins >= 7 * 24 * 60 - 60 then
				return "weekly"
			end
			if mins >= 24 * 60 then
				return ("%dd"):format(math.floor(mins / 1440 + 0.5))
			end
			return ("%dh"):format(math.max(1, math.floor(mins / 60 + 0.5)))
		end

		local function reset_text(reset_at_ms)
			if not reset_at_ms then
				return ""
			end
			local left = (reset_at_ms - mare.task.now()) / 1000
			if left <= 0 then
				return " · resets now"
			end
			if left < 3600 then
				return (" · resets in %dm"):format(math.ceil(left / 60))
			end
			if left < 48 * 3600 then
				return (" · resets in %dh %dm"):format(math.floor(left / 3600), math.floor(left % 3600 / 60))
			end
			return (" · resets in %dd"):format(math.floor(left / 86400 + 0.5))
		end

		--- quota lines per provider (shared with /status): { {text, warning} }
		function usage.quota_lines(_, _)
			-- a JSON array even when empty (the contract's result is an array)
			local out = mare.json.array()
			local names = {}
			for p in pairs(limits) do
				names[#names + 1] = p
			end
			table.sort(names)
			for _, p in ipairs(names) do
				local l = limits[p]
				for _, key in ipairs({ "primary", "secondary" }) do
					local w = l[key]
					if w and w.used_percent then
						local used = math.floor(w.used_percent + 0.5)
						out[#out + 1] = {
							text = ("%s %s: %d%% used, %d%% left%s"):format(
								p,
								window_name(w.window_minutes),
								used,
								math.max(0, 100 - used),
								reset_text(w.reset_at_ms)
							),
							warning = w.used_percent >= 90,
							gauge = w.used_percent / 100,
						}
					end
				end
				if l.reached then
					out[#out + 1] = { text = ("%s: limit reached (%s)"):format(p, tostring(l.reached)), warning = true }
				end
				if l.credits then
					out[#out + 1] = { text = ("%s credits: %s"):format(p, tostring(l.credits)) }
				end
			end
			return out
		end
		-- provided once every method of its contract is defined
		-- a mare.Service
		require("mare_lib").service(ctx, "usage", usage)

		-- registered while the optional `commands` service is up (a child fiber on the cordis tree)
		ctx:inject({ "commands" }, function(ctx)
			local function alias(n, target)
				ctx.commands:register(n, { alias_of = target })
			end
			ctx:on("app/ready", function()
				alias("cost", "usage") -- /cost alias
			end)
			ctx.commands:register("usage", {
				description = "Token usage, cost and subscription quota for this session",
				run = function()
					local function row(name, v)
						return ui.Line({ ui.Span(("%-13s"):format(name)):hl("MareMuted"), ui.Span(tostring(v)) })
					end
					---@type mare.Node[]
					local lines = {
						row(
							"requests",
							tostring(U.requests)
								.. (
									U.side_requests > 0
										and ("  (%d outside the conversation)"):format(U.side_requests)
									or ""
								)
						),
						row("input", k(U.input)),
						row(
							"output",
							k(U.output) .. (U.reasoning ~= nil and (" (%s reasoning)"):format(k(U.reasoning)) or "")
						),
						row("cache read", k(U.cache_read)),
						row(
							"cache write",
							k(U.cache_write)
								.. (U.cache_write_1h > 0 and (" (%s 1h)"):format(k(U.cache_write_1h)) or "")
						),
						row("cost", usage.cost_text(usage, ctx)),
					}
					local q = usage.quota_lines(usage, ctx)
					lines[#lines + 1] = ui.Gap()
					lines[#lines + 1] = ui.Line({ ui.Span("Quota"):hl("MareHeading") })
					if #q == 0 then
						lines[#lines + 1] = ui.Line({
							ui.Span("no quota reported yet (subscriptions report it with each response)"):hl("MareDim"),
						})
					end
					for _, l in ipairs(q) do
						local spans = {}
						if l.gauge then
							spans[#spans + 1] = ui.Span(ui.Gauge(l.gauge, 10) .. " ")
								:hl(l.warning and "MareWarning" or "MareDim")
						end
						spans[#spans + 1] = ui.Span(l.text):hl(l.warning and "MareWarning" or "MareText")
						lines[#lines + 1] = ui.Line(spans)
					end
					if ui.connected() then
						ui.overlay("Usage", ui.Lines(lines))
					end
					return U
				end,
			})
		end)
	end,
}
