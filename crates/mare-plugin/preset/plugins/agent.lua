-- agent: the agent loop. Builds requests, calls the model, dispatches tool calls, handles the
-- steer queue (inject while running) and follow-up queue (run after the current prompt), abort.
--
-- Events (emit): turn/started {turn_id, trigger, input_item_ids?, text?, source?},
--   step/started {turn_id, step_id, turn, model?, attempt?},
--   step/completed {turn_id, step_id, status, stop_reason?, usage?, error?, kind?, will_retry?, attempt?, max_attempts?, delay_ms?, reset?},
--   item/started {turn_id?, item_id, step_id?, item},
--   item/delta {turn_id, item_id, kind, delta, offset, tool_call_id?, partial?, event?},
--   item/completed {turn_id?, item_id, status, item?, item_omitted?},
--   queue/changed {items, action?},
--   turn/completed {turn_id, status, termination_reason?, cancellation_cause?, text?, structured?, usage?, error?, error_kind?, not_sent?},
--   host/notice {level, text}
-- Hooks: waterfall input/transform(msg), context/transform(messages), request/build(req),
--   tool/after(result, call); tool/rewrite, policy tool/before and tool/approval run inside
--   the host's tools:execute entry (tools plugin), which this loop calls.
--   info = {side_effect, idempotent, unconfined}. Persistence goes through the `session` SPI
--   (`persist_message`, `record_tool_state`, `record_request`, `record_request_end`,
--   `record_request_aborted`)
--   partial replies are never written piecemeal.

return {
	name = "agent",
	-- in the minimal plugin set
	tags = { "minimal" },
	provide = { "agent" },
	-- `task` gives ctx:spawn, `bus` the mare events,
	-- `policy` its decisions; session, compact and context are read when present (mare_lib.follow)
	inject = { "llm", "tools", "kernel", "task", "bus", "policy" },
	config_schema = {
		max_turns = { type = "integer", default = 200, desc = "Most model turns in one run." },
		-- concurrency-safe tool calls of one turn run in parallel, at most this many at once
		-- results are committed in the model's order
		max_parallel = {
			type = "integer",
			default = 10,
			desc = "Concurrency-safe tool calls of one turn run in parallel, at most this many at once.",
		},
		max_overflow_compactions = {
			type = "integer",
			default = 3,
			desc = "Consecutive context overflow, compact, retry rounds.",
		},
		max_image_wire_bytes = {
			type = "integer",
			default = 20 * 1024 * 1024,
			desc = "Maximum estimated inline image bytes before offloading older images.",
		},
		-- a request interrupted by a crash is retried on resume:
		-- attempts shared with network retries, backoff from 2 s doubling to 60 s
		request_retries = {
			type = "integer",
			default = 3,
			desc = "Attempts for a request interrupted by a crash, shared with network retries.",
		},
		request_retry_base_ms = { type = "integer", default = 2000, desc = "First backoff of a request retry." },
		request_retry_max_ms = { type = "integer", default = 60000, desc = "Largest backoff of a request retry." },
	},
	apply = function(ctx, cfg)
		local A = {
			messages = {},
			steer = mare.json.array(),
			follow = mare.json.array(),
			notes = {}, -- notifications (e.g. finished background jobs) for the next step
			next_turn = {}, -- custom messages queued for the next turn
			pending_custom = {}, -- custom messages deferred until the active turn ends
			busy = false,
			token = nil,
			run_id = 0,
			session_seq = 0,
			-- call id -> {state, side_effect, idempotent} (this run, or restored on resume)
			tool_states = {},
			-- persistence fault: set when a session write failed; pure
			-- conversation goes on, every tool call stops; cleared only once `pending` (the records
			-- that failed, in order) has been re-written durably
			persist_error = nil,
			pending = {},
			persist_inflight = 0,
			-- a request a resume found without a reply: {id, attempt} — the next model request
			-- reuses its id (crash count accumulates) and continues its attempt count
			resume_request = nil,
		}

		local lib = require("mare_lib")
		local D = { ctx = ctx, cfg = cfg, A = A }
		-- optional services read on each call
		D.follow = {
			session = lib.follow(ctx, "session"),
			compact = lib.follow(ctx, "compact"),
			context = lib.follow(ctx, "context"),
			subagent = lib.follow(ctx, "subagent"),
		}
		-- cold structural submodules (plugins/lua/agent/): none registers anything when loaded, so
		-- each loads the first time its field on D is read (the first prompt, queue edit or step
		-- registration) instead of during startup
		local function cold(loader)
			local _, proxy = lib.lazy_group({}, {}, loader)
			return proxy
		end
		D.persist = cold(function()
			return require("agent.persist")(D)
		end)
		D.tool_exec = cold(function()
			return require("agent.tool_exec")(D)
		end)
		D.inputs = cold(function()
			return require("agent.inputs")(D)
		end)
		D.steps = cold(function()
			return require("agent.step")(D)
		end)
		D.loop = cold(function()
			return require("agent.loop")(D)
		end)
		if mare.mode ~= "child" and mare.args and mare.args.agent then
			ctx.policy:voter("request/ready", function()
				if not D.follow.subagent() then
					return { decision = "deny", reason = "--agent requires the subagent type registry" }
				end
			end)
		end
		-- activity subscribes to turn and item events when loaded, so it stays eager
		D.activity = require("agent.activity")(D)
		require("agent.service")(D)
	end,
}
