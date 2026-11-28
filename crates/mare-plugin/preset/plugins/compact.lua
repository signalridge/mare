-- compact: staged context compaction. Measure first, avoid model calls when
-- possible, never replace what wasn't summarized, the original always stays in the session log.
--
-- (MIT / Apache-2.0)
--
-- Threshold: min(0.8 × window, window − output reserve −
-- headroom_tokens); a model whose message or pressure budget is ≤ 0 is a configuration error.
-- The prepared budget includes the request and its one parameter mapping. A later
-- provider/request listener can still change the wire cap, outside that budget.
--
-- Stages (in `stages` order; re-measured after each, stop once below the threshold):
--   prune old tool outputs → head + tail + where the original is; persisted on its own, never
--            undone
--   offload older images → attachment files referenced by path; persisted on its own, never undone
--   remote provider-side compaction (OpenAI /responses/compact)
--   local structured summary, incremental: previous summary + newer messages merged
--            with the UPDATE prompt, so the input is bounded. A request that is still too long
--            drops the oldest protocol units (a tool call with its results);
--            the previous summary is never dropped; what was dropped is recorded and said.
-- A summary stage is one atomic replacement: summarized, checked, persisted, then swapped in —
-- otherwise nothing changes.
--
-- Reject non-converging summaries before persistence and stop after three failed candidates.
--
-- Two layers: this file registers the service, hooks, commands and config and
-- keeps the cheap per-request measuring; the stages, summary requests and the run loop
-- (lua/compact/stages.lua, with prune / offload and their persistence in
-- lua/compact/projection.lua) and the per-source breakdown (lua/compact/context.lua) load on
-- first use.

return {
	name = "compact",
	inject = { "agent", "llm", "bus", "task" },
	provide = { "compact" },
	config_schema = {
		auto = { type = "boolean", default = true, desc = "Compact automatically when the threshold is reached." },
		-- trigger: min(threshold_ratio × window, window − output reserve −
		-- headroom_tokens); or an explicit percent / token count, also per model:
		-- model_thresholds = { ["openai/gpt-6.1-terra"] = { percent = 80, headroom_tokens = 32768 } }
		threshold_ratio = {
			type = "number",
			default = 0.8,
			desc = "Share of the context window that triggers compaction.",
		},
		headroom_tokens = {
			type = "integer",
			default = 32768,
			desc = "Tokens kept free of the window besides the output reserve.",
		},
		threshold_percent = { type = "number", desc = "Explicit trigger as a percent of the window." },
		threshold_tokens = { type = "number", desc = "Explicit trigger as a token count." },
		model_thresholds = { type = "map", default = {}, desc = "Per-model trigger overrides by model ref." },
		keep_recent_tokens = {
			type = "integer",
			default = 20000,
			desc = "Recent context kept verbatim after a summary.",
		},
		recent_user_tokens = {
			type = "integer",
			default = 20000,
			desc = "Older user messages kept verbatim.",
		},
		stages = {
			type = "list",
			items = { type = "string" },
			default = { "prune", "offload", "remote", "local" },
			desc = "Compaction stages, in order.",
		},
		-- prune: protect the most recent tokens; only worth it when it saves at least this much
		prune_protect_tokens = {
			type = "integer",
			default = 40000,
			desc = "Prune stage: most recent tokens left untouched.",
		},
		prune_min_saving_tokens = {
			type = "integer",
			default = 20000,
			desc = "Prune stage: least saving that makes it worth running.",
		},
		-- tool-result pruner defaults (code points): outputs over 8192 keep the first 4096 and
		-- the last 1024
		prune_tool_result_chars = {
			type = "integer",
			default = 8192,
			desc = "Tool results longer than this are pruned.",
		},
		prune_head_chars = {
			type = "integer",
			default = 4096,
			desc = "Characters kept from the head of a pruned result.",
		},
		prune_tail_chars = {
			type = "integer",
			default = 1024,
			desc = "Characters kept from the tail of a pruned result.",
		},
		offload_keep_images = { type = "integer", default = 2, desc = "The most recent images stay inline." },
		remote = {
			type = "boolean",
			default = true,
			desc = "Run the provider-side compaction stage (can also be removed from `stages`).",
		},
		summary_model = { type = "string", desc = "Model ref for local summaries, e.g. a cheaper one." },
		summary_max_tokens = { type = "number", desc = "Output cap of a summary request; default headroom." },
		failure_backoff_ms = { type = "integer", default = 30000, desc = "Initial backoff after a failed compaction." },
		failure_backoff_max_ms = {
			type = "integer",
			default = 600000,
			desc = "Largest backoff after repeated failures.",
		},
		-- scale the retained tail with the model's message budget
		retain_ratio = {
			type = "number",
			default = 0.16,
			desc = "Share of the message budget retained after a summary.",
		},
	},
	apply = function(ctx, cfg)
		-- mutable state shared with the implementation modules (lua/compact/*)
		local S = {
			busy = false,
			---@type mare.Token|nil
			token = nil, -- cancellation for the running stage (Esc / abort)
			version = 0, -- context version: bumped whenever compaction changes the context
			usage_version = -1, -- the version the provider-reported usage belongs to
			warned_near = false,
			last_compacted = nil, -- suppress another auto run until the context grows
			stalled = false, -- circuit open until the context has room or is reset
			fail_backoff = nil,
			retry_at = 0,
		}
		function S.reset_compaction_guard()
			S.warned_near = false
			S.last_compacted = nil
			S.stalled = false
			S.fail_backoff = nil
			S.retry_at = 0
		end
		-- the optional services, read on each call: S.svc.<name> is the
		-- service while it is up, nil otherwise (each held by a follower fiber)
		local mare_lib = require("mare_lib")
		S.svc = mare_lib.followers(ctx, { "session", "usage", "context", "tools", "todo" })
		-- Every path that clears `busy` calls this: work deferred while compacting (the agent's
		-- woken notifications) waits on `compact/idle`, not on a result event some paths skip.
		-- Spawned: callers include `<close>` handlers, which must not yield.
		function S.released()
			ctx:spawn(function()
				if not S.busy then
					ctx.bus:emit("compact/idle", {})
				end
			end)
		end

		-- Synchronous config validation during `apply` via the lightweight shared budget module.
		local budget = require("compact.budget")
		local function is_headroom_explicit()
			local raw = mare.config and mare.config.plugins and mare.config.plugins.compact
			return type(raw) == "table" and raw.headroom_tokens ~= nil
		end
		S.headroom_explicit = is_headroom_explicit
		local valid, config_error = budget.validate_config(cfg)
		if not valid then
			error(config_error, 0)
		end

		-- Cold submodules load on first real use, keeping `compact:apply` free of heavy requires.
		local metrics_mod
		local function get_metrics()
			if not metrics_mod then
				metrics_mod = require("compact.measure").create(ctx, cfg, S)
			end
			return metrics_mod
		end
		local svc_mod, run_compact_fn
		local function get_service()
			if not svc_mod then
				svc_mod, run_compact_fn = require("compact.service").create(ctx, cfg, S, get_metrics())
			end
			return svc_mod, run_compact_fn
		end
		local hooks_mod
		local compact = {}
		local function get_hooks()
			if not hooks_mod then
				local _, run_compact = get_service()
				hooks_mod = require("compact.hooks").create(ctx, cfg, S, get_metrics(), compact, run_compact)
			end
			return hooks_mod
		end

		local function check_model_budget(model)
			if not model then
				return
			end
			local _, err = budget.compute(cfg, model, nil, is_headroom_explicit())
			if err then
				ctx.bus:emit("host/notice", { level = "error", text = err })
				if mare.ui.connected() then
					ui.notify(err, "error")
				end
			end
		end

		ctx:on("turn/started", function(ev)
			S.active_turn_id = type(ev) == "table" and ev.turn_id or nil
		end)
		ctx:on("turn/completed", function()
			S.active_turn_id = nil
		end)
		ctx:on("item/completed", function(ev)
			local m = ev and ev.item and ev.item.message
			if m and m.role == "assistant" and m.usage then
				S.usage_version = S.version
				S.usage_model = type(m.provider_id) == "string"
						and type(m.model_id) == "string"
						and (m.provider_id .. "/" .. m.model_id)
					or nil
			end
		end)
		ctx:on("session/new", function()
			S.warned_near = false
			S.version = S.version + 1
			S.usage_model = nil
			S.reset_compaction_guard()
		end)
		ctx:on("session/loaded", function(ev)
			S.reset_compaction_guard()
			-- A CLI override or a compaction/projection in the session log after the last
			-- assistant usage invalidates old provider usage so it must not become current again
			-- when session restoration completes.
			S.usage_model = nil
			if mare_lib.session_usage_stale(S.svc.session, ev) then
				S.usage_version = -1
				if S.svc.usage then
					local m = get_metrics()
					local ag = ctx.agent
					local msgs = (ev and ev.messages) or ag:messages()
					local ok, oh = pcall(m.overhead)
					S.svc.usage:set_context(m.estimate(msgs) + (ok and oh or 0))
				end
			else
				S.usage_version = S.version
				local msgs = (ev and ev.messages) or ctx.agent:messages()
				for i = #msgs, 1, -1 do
					local m = msgs[i] --[[@as table<string, any>]]
					if m.role == "assistant" and m.usage then
						if type(m.provider_id) == "string" and type(m.model_id) == "string" then
							S.usage_model = m.provider_id .. "/" .. m.model_id
						end
						break
					end
				end
			end
		end)
		ctx:on("llm/model-changed", function(ev)
			if not (type(ev) == "table" and ev.params_only) then
				S.reset_compaction_guard()
				check_model_budget(ev and ev.model)
			end
		end)
		ctx:on("llm/login", function()
			S.stalled = false
			S.fail_backoff = nil
			S.retry_at = 0
		end)
		ctx:on("llm/providers-changed", function()
			S.stalled = false
			S.fail_backoff = nil
			S.retry_at = 0
		end)
		ctx:on("llm/model-changing", function(ev)
			return get_hooks().on_model_changing(ev)
		end, { timeout_ms = false })
		ctx:on("compact/overflow", function(req)
			return get_hooks().on_overflow(req)
		end)
		ctx:on("compact/started", function()
			if mare.ui.connected() then
				mare.ui.working("Compacting conversation")
			end
		end)
		ctx:on("compact/completed", function(ev)
			return get_hooks().on_completed(ev)
		end)
		-- Esc / Ctrl+C stop a running compaction without taking over normal interrupts when idle.
		ctx:inject({ "ui" }, function(sub)
			sub.ui:handler("interrupt", "compact", { order = 20 }, function()
				if S.token and not S.token:is_cancelled() then
					S.token:cancel()
					return true
				end
				return false
			end)
		end)
		ctx:inject({ "commands" }, function(sub)
			sub.commands:register("compact", {
				description = "Compact the conversation (/compact [focus])",
				availability = "idle",
				run = function(args)
					return get_hooks().run_command(args)
				end,
			})
		end)
		-- Report an invalid configured model budget once the app is ready (and immediately on
		-- `/reload` after startup has already completed) without loading cold modules.
		local APP_READY_KEY = "compact.app_ready"
		local budget_checked = false
		local function schedule_model_budget_check()
			if budget_checked then
				return
			end
			budget_checked = true
			ctx:spawn(function()
				local okm, m = pcall(ctx.llm.current, ctx.llm)
				if okm and m then
					check_model_budget(m)
				end
			end)
		end
		ctx:on("app/ready", function()
			mare.g[APP_READY_KEY] = true
			schedule_model_budget_check()
		end)
		ctx:on("reload/completed", function()
			mare.g[APP_READY_KEY] = true
			schedule_model_budget_check()
		end)
		if mare.g[APP_READY_KEY] then
			schedule_model_budget_check()
		end

		compact.register_stage = function(self, caller, name, fn, opts)
			return get_service().register_stage(self, caller, name, fn, opts)
		end
		compact.measure = function(self, caller, opts)
			return get_service().measure(self, caller, opts)
		end
		compact.threshold = function(_, _, model)
			if not cfg.auto then
				return nil
			end
			return (get_metrics().threshold(model))
		end
		compact.offload_images = function(self, caller, opts)
			return get_service().offload_images(self, caller, opts)
		end
		compact.cancel = function(_, _)
			if S.token then
				S.token:cancel()
			end
		end
		compact.busy = function(_, _)
			return S.busy
		end
		compact.run = function(self, caller, instructions, opts)
			return get_service().run(self, caller, instructions, opts)
		end
		compact.prepare_request = function(_, _, req, history)
			return get_hooks().prepare_request(req, history)
		end
		mare_lib.service(ctx, "compact", compact)
	end,
}
