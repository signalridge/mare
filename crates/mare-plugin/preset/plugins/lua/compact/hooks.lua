-- compact.hooks: model admission, auto-compaction transform, UI progress and /compact;
-- part of the `compact` plugin (plugins/compact.lua).
local M = {}

local function make_model_admission(ctx, S, metrics, run_compact)
	-- Admission precedes changes to the model, pinned parameters and saved defaults.
	return function(ev)
		-- refusals are expected outcomes: ev.refuse makes them the declared llm.model_unavailable
		-- (a dispatch without it, such as a resume, raises them as they are)
		local refuse = ev.refuse or function(message)
			error(message, 0)
		end
		-- The loaded context already belongs to the restored model. An explicit override
		-- is admitted separately, after restoring that model as the summarization source.
		if ev.restoring then
			return
		end
		if S.busy or S.target_model then
			refuse("cannot switch models while compacting")
		end
		if not ev.source_unknown and (not ev.previous or ev.previous.ref == ev.model.ref) then
			return
		end
		local b, err = metrics.budgets(ev.model)
		if not b then
			metrics.check_model(ev.model)
			refuse(tostring(err))
		end
		-- An explicit auto threshold cannot waive the target request pressure budget.
		b.limit = math.min(b.limit, b.pressure)
		local ag = ctx.agent
		local msgs = ag:messages()
		local incompatible_remote = false
		for _, m in ipairs(msgs) do
			local raw = m --[[@as table]]
			if raw.compaction and type(raw.remote) == "table" then
				if raw.remote.provider_id ~= ev.model.provider or raw.remote.model_id ~= ev.model.id then
					incompatible_remote = true
					break
				end
			end
		end
		if not incompatible_remote and metrics.estimate(msgs) + metrics.overhead(ev.model, true) < b.limit then
			return
		end
		if ev.source_unknown then
			refuse(
				"Cannot switch to "
					.. ev.model.ref
					.. ": context exceeds the target budget and no source model is recorded; select a larger context window or restore an explicit source model first"
			)
		end
		local original = ag:messages()
		local hash = mare.crypto.sha256(mare.json.encode(original))
		local cleanup <close> = setmetatable({}, {
			__close = function()
				if S.token then
					S.token:cancel()
				end
				S.target_model, S.target_limit, S.transaction, S.token, S.busy = nil, nil, nil, nil, false
				S.released()
			end,
		})
		S.target_model, S.target_limit = ev.model, b.limit
		S.transaction = {
			messages = table.copy(original),
			original = table.copy(original),
			hash = hash,
			origins = {},
			projections = {},
			model = { ref = ev.model.ref, effort = ev.effort },
			target_model = ev.model,
		}
		local called, res = pcall(run_compact, nil, { reason = "model_switch" })
		local ok = called and type(res) == "table" and (res.status == "compacted" or res.status == "unchanged")
		local why = nil
		if called and ok and metrics.estimate(S.transaction.messages) + metrics.overhead(ev.model, true) >= b.limit then
			ok, why = false, "the context is still above the target model threshold"
		elseif not called then
			-- compaction reporting `not_compacted` is an expected refusal and says why; compaction
			-- raising an unexpected internal error goes on as it is (outside callers see `internal`)
			if type(res) == "table" and res.code == "compact.not_compacted" then
				why = res.message
			else
				error(res, 0)
			end
		elseif not ok then
			why = "nothing to compact"
		end
		if not ok then
			refuse(
				"Cannot switch to "
					.. ev.model.ref
					.. ": context compaction failed: "
					.. tostring(why)
					.. ". The previous model is preserved; retry or select a larger context window."
			)
		end
		local tx = S.transaction
		assert(ev.stage, "model admission requires a staged commit")
		ev.stage(function(publish)
			S.transaction = tx
			local tx_cleanup <close> = setmetatable({}, {
				__close = function()
					S.transaction = nil
				end,
			})
			assert(tx.commit, "model admission did not prepare a context commit")
			tx.commit(publish)
		end)
	end
end

local function make_prepare_request(ctx, cfg, S, metrics, run_compact)
	-- Only the prepared request can account for additions made by request/build and tool binding.
	return function(req, history)
		local msgs, model = req.messages, req.model
		if S.busy then
			return { messages = msgs, compacted = false }
		end
		if cfg.auto and model then
			local reserved = req.params_overlay and req.params_overlay.reserved or req.max_tokens
			local budget, err = metrics.budgets(model, reserved)
			if not budget then
				error(err, 0)
			end
			local limit = budget.limit
			local local_used, breakdown = metrics.request_tokens(req)
			local used = local_used
			if S.svc.usage and S.usage_version == S.version and S.usage_model == model.ref then
				used = math.max(used, S.svc.usage:context_tokens() or 0)
			end
			if S.stalled and limit and used <= limit * 0.9 then
				S.reset_compaction_guard()
			end
			if
				limit
				and used > limit
				and not S.stalled
				and (not S.last_compacted or local_used > S.last_compacted)
				and mare.task.clock() >= (S.retry_at or 0)
			then
				local lib = require("mare_lib")
				local current = ctx.agent:messages()
				local same = lib.equal_messages(current, history)
				local start = same and lib.find_history(msgs, history) or nil
				if not start then
					ctx.bus:emit("host/notice", {
						level = "warning",
						text = "Automatic compaction skipped: the prepared request cannot be mapped uniquely to the current history",
					})
					return { messages = msgs, compacted = false }
				end
				S.prepared = {
					model = model,
					budget = budget,
					overhead = local_used - metrics.estimate(history),
					breakdown = breakdown,
					system = req.system,
					tools = req.tools,
					history = lib.copy_deep(history),
				}
				local cleanup <close> = setmetatable({}, {
					__close = function()
						S.prepared = nil
					end,
				})
				local version = S.version
				local called, res = pcall(run_compact, nil, { reason = "auto", cancel = req.cancel })
				local after = ctx.agent:messages()
				local changed = S.version ~= version
					or (
						called
						and type(res) == "table"
						and res.status == "compacted"
						and not lib.equal_messages(after, history)
					)
				if changed then
					msgs = lib.replace_history(msgs, start, #history, after)
				end
				if called and type(res) == "table" and (res.status == "compacted" or res.status == "unchanged") then
					S.fail_backoff, S.retry_at = nil, 0
					S.last_compacted = metrics.estimate(ctx.agent:messages()) + S.prepared.overhead
				else
					S.fail_backoff =
						math.min((S.fail_backoff or cfg.failure_backoff_ms / 2) * 2, cfg.failure_backoff_max_ms)
					S.retry_at = mare.task.clock() + S.fail_backoff
				end
				return { messages = msgs, compacted = changed }
			elseif limit and used > limit * 0.9 and not S.warned_near then
				S.warned_near = true
				ctx.bus:emit("compact/approaching", { tokens = used, threshold = limit })
				if mare.ui.connected() then
					ui.notify("Context is nearly full — it will be compacted soon", "info")
				end
			end
		end
		return { messages = msgs, compacted = false }
	end
end

function M.create(ctx, cfg, S, metrics, compact, run_compact)
	local on_model_changing = make_model_admission(ctx, S, metrics, run_compact)
	local prepare_request = make_prepare_request(ctx, cfg, S, metrics, run_compact)

	local function on_overflow(req)
		local called, res = pcall(run_compact, nil, {
			cancel = req and req.cancel,
			reason = "overflow",
		})
		return { handled = true, succeeded = called, res = res }
	end

	local function on_completed(ev)
		if ev.reason ~= "auto" and ev.status == "completed" then
			S.reset_compaction_guard()
			S.last_compacted = metrics.estimate(ctx.agent:messages()) + metrics.overhead()
		end
		if not mare.ui.connected() then
			return
		end
		if ev.cancelled then
			-- the user's own cancel: a quiet line, never an error popup
			ui.notify("Compaction cancelled", "info")
			return
		end
		if ev.error then
			local msg = require("mare_lib").error_message(ev.error) or tostring(ev.error)
			if ev.changed then
				ui.notify("Compaction incomplete: " .. msg, "warning")
			else
				ui.notify("Compaction failed: " .. msg, "error")
			end
			return
		end
		local id = mare.ui.push("info", {
			node = ui.Line({
				ui.Span("◆ "):hl("MareMark"),
				ui.Span(
					("Compacted conversation (%dk → %dk tokens)"):format(
						ev.tokens_before // 1000,
						ev.tokens_after // 1000
					)
				):hl("MareMuted"),
			}),
			expanded = ev.summary and ui.Markdown(ev.summary) or nil,
		})
		mare.ui.finish(id)
	end

	local function run_command(args)
		if ctx.agent:activity().busy then
			ui.notify("Wait for the current run to finish", "warning")
			return false, "Wait for the current run to finish"
		end
		local session = S.svc.session
		local owner = session and session.info and session:info().session_id or nil
		ctx:spawn(function()
			if
				owner
				and (not session or session:info().session_id ~= owner or (session.switching and session:switching()))
			then
				return
			end
			local ok, res = pcall(compact.run, compact, ctx, args)
			if not ok then
				local kind = type(res) == "table" and res.data and res.data.kind or nil
				if kind ~= "cancelled" then
					local msg = type(res) == "table" and res.message or res
					ui.notify("Compaction skipped: " .. tostring(msg), "warning")
				end
			elseif type(res) == "table" and res.status == "nothing_to_compact" then
				ui.notify("Compaction skipped: nothing to compact", "warning")
			end
			mare.ui.working(nil)
		end)
	end

	return {
		on_model_changing = on_model_changing,
		prepare_request = prepare_request,
		on_overflow = on_overflow,
		on_completed = on_completed,
		run_command = run_command,
	}
end

return M
