-- compact.service: stage registry, lazy stage/breakdown loaders, and `compact` service;
-- part of the `compact` plugin (plugins/compact.lua).
local M = {}

local function make_register_stage(stages, SUMMARY_STAGES)
	local base_stages = {
		prune = stages.prune,
		offload = stages.offload,
		remote = stages.remote,
		["local"] = stages["local"],
	}
	local base_summary = {
		remote = true,
		["local"] = true,
	}
	local stage_stacks = {}
	local function sync_stage(name)
		local stack = stage_stacks[name]
		local top = stack and stack[#stack]
		if top then
			stages[name] = top.fn
			SUMMARY_STAGES[name] = top.summary
		else
			stages[name] = base_stages[name]
			SUMMARY_STAGES[name] = base_summary[name]
		end
	end

	return function(_, caller, name, fn, opts)
		local stack = stage_stacks[name]
		if not stack then
			stack = {}
			stage_stacks[name] = stack
		end
		local entry = {
			fn = fn,
			summary = (opts and opts.summary) and true or nil,
			disposed = false,
		}
		stack[#stack + 1] = entry
		sync_stage(name)
		-- the stage goes with whoever registered it (the caller's fiber); the release is the
		-- effect's disposer
		local function dispose()
			if entry.disposed then
				return
			end
			entry.disposed = true
			local current = stage_stacks[name]
			if current then
				for i = #current, 1, -1 do
					if current[i] == entry then
						table.remove(current, i)
						break
					end
				end
				if #current == 0 then
					stage_stacks[name] = nil
				end
			end
			sync_stage(name)
		end
		if caller and caller.effect then
			return caller:effect(function()
				return dispose
			end, "compact stage " .. name)
		end
		return dispose
	end
end

function M.create(ctx, cfg, S, metrics)
	-- registry: name → fn(state) -> changed, reason (plugins add or replace stages); the
	-- built-in stages load lua/compact/stages.lua when they first run
	local stages, SUMMARY_STAGES = {}, { remote = true, ["local"] = true }
	local impl
	local function I()
		if not impl then
			impl = require("compact.stages")({
				ctx = ctx,
				cfg = cfg,
				S = S,
				tokens = metrics.tokens,
				msg_tokens = metrics.msg_tokens,
				estimate = metrics.estimate,
				measure = metrics.measure,
				overhead = metrics.overhead,
				changed = metrics.changed,
				notify = metrics.notify,
				budgets = metrics.budgets,
				threshold = metrics.threshold,
				stages = stages,
				SUMMARY_STAGES = SUMMARY_STAGES,
			})
		end
		return impl
	end
	stages.prune = function(st)
		return I().prune(st)
	end
	stages.offload = function(st)
		return I().offload(st)
	end
	stages.remote = function(st)
		return I().remote(st)
	end
	stages["local"] = function(st)
		return I().local_(st)
	end

	-- measure({ by_source = true }) loads lua/compact/context.lua
	local by_source
	local function breakdown_mod()
		if not by_source then
			by_source = require("compact.context")({
				ctx = ctx,
				S = S,
				tokens = metrics.tokens,
				msg_tokens = metrics.msg_tokens,
				overhead = metrics.overhead,
			})
		end
		return by_source
	end

	local compact = {}

	--- register_stage(name, fn, opts?) → plugins add a stage or replace a built-in one;
	--- opts.summary = true makes it count as a summary stage. Order comes from cfg.stages.
	compact.register_stage = make_register_stage(stages, SUMMARY_STAGES)

	--- measure(opts?) -> tokens the current context uses (provider-reported or estimated);
	--- measure({ by_source = true }) -> the per-source breakdown {system, instructions, skills,
	--- tools, messages, tool_results, images, total} (lua/compact/context.lua)
	function compact.measure(_, _, opts)
		if type(opts) == "table" and opts.by_source then
			return breakdown_mod().breakdown()
		end
		return metrics.measure()
	end

	--- threshold(model?) -> tokens at which auto compaction triggers for `model` (default: the
	--- current one), or nil when auto compaction is off or the model's budgets are invalid
	---
	function compact.threshold(_, _, model)
		if not cfg.auto then
			return nil
		end
		return (metrics.threshold(model))
	end

	--- offload_images(opts?) -> count | nil, err: the offload stage on its own, for a
	--- provider's image-limit error (too many / too large images go through
	--- image offload, then the request is sent again). opts.keep: images left inline
	--- (default offload_keep_images; 0 = all). Persisted like the stage; nothing to offload → 0.
	function compact.offload_images(_, _, opts)
		opts = opts or {}
		if S.busy then
			return nil, "already compacting"
		end
		if not stages.offload then
			return nil, "no offload stage"
		end
		local st = { reason = "image_limit", keep_images = opts.keep }
		local ok, err = stages.offload(st)
		if st.stop then
			return nil, st.stop
		end
		return ok and (st.offloaded or 1) or 0, (not ok) and err or nil
	end

	--- cancel(): cancel a running compaction
	function compact.cancel(_, _)
		if S.token then
			S.token:cancel()
		end
	end

	--- Optional lifecycle capability for session switching.
	function compact.busy(_, _)
		return S.busy
	end

	local function call_run(instructions, opts)
		local res, err, kind = I().run(instructions, opts)
		if res == nil then
			require("mare_lib").fail(
				"compact.not_compacted",
				tostring(err or "compaction failed"),
				kind ~= nil and { kind = tostring(kind) } or {}
			)
		end
		return res
	end

	--- run(instructions?, opts?) → {status, tokens_before, tokens_after, summary?, dropped?}
	--- Public entrypoint: accepts manual compaction or `summarize_only` (`reason` may only be `"manual"`).
	function compact.run(_, _, instructions, opts)
		if opts and opts.reason ~= nil and opts.reason ~= "manual" then
			require("mare_lib").fail("invalid_arguments", "compact.run only accepts manual compaction")
		end
		return call_run(instructions, opts)
	end

	return compact, call_run
end

return M
