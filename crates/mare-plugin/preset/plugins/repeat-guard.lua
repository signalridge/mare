-- repeat-guard: advisory consecutive-repeat detector (MIT): the
-- same tool with the same (canonical) arguments called N times in a row gets a reminder for the
-- model at the configured thresholds (default 3, 5, 8). It never refuses or rewrites a call.
-- A user message (new prompt or steer) resets the chain — repetition across it is not a loop.
-- Calls of tools outside `include` / inside `exclude` are transparent (neither count nor reset).

-- Reminder strings
local GENTLE = "You are repeating the exact same tool call with identical arguments. "
	.. "Carefully analyze the previous result before calling again: if the task is "
	.. "not complete, try a different approach or different arguments instead of "
	.. "repeating the call."

local function detailed(name, count, args)
	return "Repeated tool call detected:\n"
		.. ("- tool: %s\n"):format(name)
		.. ("- consecutive_calls: %d\n"):format(count)
		.. ("- arguments: %s\n"):format(args)
		.. "The repeated calls are not making progress. Do not call this tool with "
		.. "these exact arguments again. Inspect the latest result and choose a "
		.. "different action, different arguments, or finish the task if enough "
		.. "evidence has been gathered."
end

local function wildcard(p)
	return "^" .. p:gsub("[%^%$%(%)%%%.%[%]%+%-%?]", "%%%0"):gsub("%*", ".*") .. "$"
end

return {
	name = "repeat-guard",
	-- `bus` for its documented mare event
	inject = { "agent", "bus" },
	config_schema = {
		thresholds = {
			type = "list",
			items = { type = "integer" },
			default = { 3, 5, 8 },
			desc = "Repeat counts at which the guard steps in.",
		},
		include = {
			type = "list",
			items = { type = "string" },
			default = {},
			desc = "Tool-name patterns (`*` wildcard) to guard; empty = every tool.",
		},
		exclude = {
			type = "list",
			items = { type = "string" },
			default = {},
			desc = "Tool-name patterns (`*` wildcard) never guarded.",
		},
		arguments_preview_chars = {
			type = "integer",
			default = 500,
			desc = "Bounds the quoted arguments, never the detection.",
		},
	},
	apply = function(ctx, cfg)
		local thresholds = {}
		for _, n in ipairs(cfg.thresholds or {}) do
			assert(math.type(n) == "integer" and n >= 2, "repeat-guard: every threshold must be an integer >= 2")
			thresholds[#thresholds + 1] = n
		end
		assert(#thresholds > 0, "repeat-guard: `thresholds` must not be empty")
		table.sort(thresholds)
		local is_threshold = {}
		for _, n in ipairs(thresholds) do
			assert(not is_threshold[n], "repeat-guard: `thresholds` must not contain duplicates")
			is_threshold[n] = true
		end
		local include, exclude = {}, {}
		for _, p in ipairs(cfg.include or {}) do
			include[#include + 1] = wildcard(p)
		end
		for _, p in ipairs(cfg.exclude or {}) do
			exclude[#exclude + 1] = wildcard(p)
		end
		local function tracked(name)
			if #include > 0 then
				local any = false
				for _, pat in ipairs(include) do
					any = any or name:find(pat) ~= nil
				end
				if not any then
					return false
				end
			end
			for _, pat in ipairs(exclude) do
				if name:find(pat) then
					return false
				end
			end
			return true
		end

		local chain = nil -- {key, count}
		local function reset()
			chain = nil
		end
		ctx:on("turn/started", reset)
		ctx:on("queue/changed", function(ev)
			if ev and ev.action == "enqueued" then
				reset()
			end
		end)

		ctx:on("item/completed", function(ev)
			local item = ev and ev.item
			local call = item and item.type == "tool_call" and item or nil
			if not call or type(call.name) ~= "string" or not tracked(call.name) then
				return
			end
			-- canonical arguments: Lua→JSON sorts object keys, so key order never matters
			local canonical = type(call.arguments) == "string" and call.arguments
				or mare.json.encode(call.arguments or {})
			local key = call.name .. "\0" .. canonical
			local count = (chain and chain.key == key) and chain.count + 1 or 1
			chain = { key = key, count = count }
			if not is_threshold[count] then
				return
			end
			local text
			if count == thresholds[1] then
				text = GENTLE
			else
				local preview = canonical
				local cap = cfg.arguments_preview_chars
				local len = utf8.len(canonical) or #canonical
				if len > cap then
					preview = mare.text.cut(preview, cap, { unit = "chars" })
						.. ("… (+%d more chars)"):format(len - cap)
				end
				text = detailed(call.name, count, preview)
			end
			ctx.bus:emit("repeat-guard/reminder", { tool = call.name, count = count })
			ctx.agent:notify(text)
		end)
	end,
}
